//! Which pin decided: the two pin legs of the route-slot walk and the
//! effort-only attach, one module so the rung order has one home.
//!
//! A typed flag is the operator's live statement and outranks everything;
//! a node's own pin is the standing ruling one rung below it. Both
//! override the lanes; the chain names which pin decided so --explain
//! reads the verdict. The transport projects the pins, so nothing here
//! reads a graph.

use serde_json::{json, Map, Value};

use crate::route_slot::{none, refused_decision, row_value, validated_effort};

/// The typed-flag leg: a typed model maps to its harness through the
/// declared rows; a typed route or vendor keeps the plain override. Every
/// arm returns, so a pin never borrows a lane's capacity.
pub(crate) fn operator_pin_leg(
    payload: &Value,
    explicit_model_name: Option<String>,
    explicit_route_name: Option<String>,
    explicit_vendor_name: Option<String>,
    explicit_lane: bool,
    chain: &mut Vec<Value>,
) -> Option<Value> {
    // A typed --model whose routing.models row declares exactly one
    // harness resolves that row instead of falling through to a
    // config-scalar harness: the row IS the model's own
    // declaration. A typed --route or -P, or a typed -H (payload
    // explicit_lane), keeps the plain override - the operator already
    // named those axes. Zero row matches also keep it: no vendor
    // inference here.
    let model_only =
        explicit_route_name.is_none() && explicit_vendor_name.is_none() && !explicit_lane;
    let matched: Vec<(String, Value)> = if model_only {
        let model = explicit_model_name.as_deref().unwrap_or("");
        payload
            .get("declared_rows")
            .and_then(Value::as_object)
            .map(|rows| {
                rows.iter()
                    .filter(|(_, row)| row_value(row, "model") == model)
                    .map(|(name, row)| (name.clone(), row.clone()))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let declared_harnesses: Vec<String> = matched
        .iter()
        .map(|(_, row)| row_value(row, "harness"))
        .filter(|h| !h.is_empty())
        .collect();
    let harness_set: std::collections::BTreeSet<String> =
        declared_harnesses.iter().cloned().collect();
    if model_only && harness_set.len() > 1 {
        let model = explicit_model_name.as_deref().unwrap_or("");
        let list = matched
            .iter()
            .map(|(name, row)| format!("{} (harness {})", name, row_value(row, "harness")))
            .collect::<Vec<_>>()
            .join(" and ");
        let remedies = {
            let mut hs: Vec<&str> = declared_harnesses.iter().map(|s| s.as_str()).collect();
            hs.sort();
            hs.dedup();
            hs.iter()
                .map(|h| format!("-H {h}"))
                .collect::<Vec<_>>()
                .join(" or ")
        };
        chain.push(json!(
            "slot=operator-pin-override (a typed model/vendor/route outranks the lanes)"
        ));
        chain.push(json!(format!(
            "slot=strict-refusal --model {model} matches routing.models rows {list}; pass {remedies}"
        )));
        return Some(refused_decision(
            std::mem::take(chain),
            "pin-model-ambiguous-harness",
            "the typed model's declared rows name different harnesses",
        ));
    }
    if model_only && harness_set.len() == 1 {
        let (row_name, row) = &matched[0];
        let harness = declared_harnesses[0].clone();
        let mut out = Map::new();
        out.insert("harness".into(), json!(harness));
        out.insert("model".into(), json!(explicit_model_name.clone().unwrap()));
        out.insert("pin_row".into(), json!(row_name));
        let route_set: std::collections::BTreeSet<String> = matched
            .iter()
            .map(|(_, row)| row_value(row, "route"))
            .filter(|s| !s.is_empty())
            .collect();
        if route_set.len() == 1 {
            out.insert(
                "route".into(),
                json!(route_set.iter().next().unwrap().to_string()),
            );
        }
        let account_set: std::collections::BTreeSet<String> = matched
            .iter()
            .map(|(_, row)| row_value(row, "account"))
            .filter(|s| !s.is_empty())
            .collect();
        if account_set.len() == 1 {
            let account = account_set.iter().next().unwrap().clone();
            out.insert("account".into(), json!(account));
        }
        let effort_ok = payload.get("effort_ok").cloned().unwrap_or(json!({}));
        validated_effort(
            &mut out,
            &harness,
            &row_value(row, "effort"),
            &effort_ok,
            chain,
            "pin",
        );
        chain.push(json!(format!(
            "slot=operator-pin-override row={row_name} harness={harness} (the typed model's declared row names its harness)"
        )));
        return Some(json!({
            "status": "pick",
            "candidate": Value::Object(out),
            "chain": chain,
        }));
    }
    chain.push(json!(
        "slot=operator-pin-override (a typed model/vendor/route outranks the lanes)"
    ));
    Some(none(std::mem::take(chain)))
}

/// The node-pin leg: the standing ruling one rung below a typed flag. A
/// pinned model maps to its harness through the declared rows exactly
/// like a typed model; with no declared row the walk keeps today's lane
/// answer and names the fact, because the spawn seam still rides the pin
/// as its --model. `None` = no node pin, the lanes decide.
pub(crate) fn node_pin_leg(payload: &Value, chain: &mut Vec<Value>) -> Option<Value> {
    let node = payload.get("node").cloned().unwrap_or(json!({}));
    let np = |k: &str| -> Option<String> {
        node.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let pin_model = np("model");
    let pin_provider = np("provider");
    if pin_model.is_none() && pin_provider.is_none() {
        return None;
    }
    let effort_ok = payload.get("effort_ok").cloned().unwrap_or(json!({}));
    if let Some(model) = pin_model {
        let matched: Vec<(String, Value)> = payload
            .get("declared_rows")
            .and_then(Value::as_object)
            .map(|rows| {
                rows.iter()
                    .filter(|(_, row)| row_value(row, "model") == model)
                    .map(|(name, row)| (name.clone(), row.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let declared_harnesses: Vec<String> = matched
            .iter()
            .map(|(_, row)| row_value(row, "harness"))
            .filter(|h| !h.is_empty())
            .collect();
        let harness_set: std::collections::BTreeSet<String> =
            declared_harnesses.iter().cloned().collect();
        if harness_set.len() > 1 {
            let list = matched
                .iter()
                .map(|(name, row)| format!("{} (harness {})", name, row_value(row, "harness")))
                .collect::<Vec<_>>()
                .join(" and ");
            chain.push(json!(
                "slot=node-pin (a node pin outranks the lanes, one rung below a typed flag)"
            ));
            chain.push(json!(format!(
                "slot=strict-refusal --model {model} matches routing.models rows {list}; pass -H <harness> to disambiguate"
            )));
            return Some(refused_decision(
                std::mem::take(chain),
                "pin-model-ambiguous-harness",
                "the node-pinned model's declared rows name different harnesses",
            ));
        }
        if harness_set.len() == 1 {
            let (row_name, _row) = &matched[0];
            let harness = declared_harnesses[0].clone();
            let mut out = Map::new();
            out.insert("harness".into(), json!(harness));
            out.insert("model".into(), json!(model));
            out.insert("pin_row".into(), json!(row_name));
            let route_set: std::collections::BTreeSet<String> = matched
                .iter()
                .map(|(_, row)| row_value(row, "route"))
                .filter(|s| !s.is_empty())
                .collect();
            if route_set.len() == 1 {
                out.insert(
                    "route".into(),
                    json!(route_set.iter().next().unwrap().clone()),
                );
            }
            let account_set: std::collections::BTreeSet<String> = matched
                .iter()
                .map(|(_, row)| row_value(row, "account"))
                .filter(|s| !s.is_empty())
                .collect();
            if account_set.len() == 1 {
                out.insert(
                    "account".into(),
                    json!(account_set.iter().next().unwrap().clone()),
                );
            }
            validated_effort(
                &mut out,
                &harness,
                &np("effort").unwrap_or_default(),
                &effort_ok,
                chain,
                "node-pin",
            );
            chain.push(json!(format!(
                "slot=node-pin row={row_name} harness={harness} (the node's pinned model's declared row names its harness)"
            )));
            return Some(json!({
                "status": "pick",
                "candidate": Value::Object(out),
                "chain": chain,
            }));
        }
        // No declared row names the pinned model: the lanes decide the
        // harness and the spawn seam still rides the pin as --model.
        chain.push(json!(format!(
            "slot=node-pin model={model} kept (no routing.models row names it; lanes decide the harness)"
        )));
    }
    if let Some(harness_pin) = pin_provider {
        chain.push(json!(format!(
            "slot=node-pin harness={harness_pin} kept (the spawn seam rides the pin; lanes still decide capacity)"
        )));
    }
    None
}

/// An effort-only node pin attaches to whatever candidate the walk armed:
/// the model/provider pins reroute before the lanes, but an effort pin
/// only ever rides the winner. The surface table validates it; no surface
/// on the harness drops the pin by name.
pub(crate) fn attach_effort_pin(decision: &mut Value, payload: &Value) {
    let node_effort = payload
        .get("node")
        .and_then(|n| n.get("effort"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("");
    if node_effort.is_empty() {
        return;
    }
    let Some(obj) = decision.as_object_mut() else {
        return;
    };
    let Some(cand) = obj.get_mut("candidate").and_then(Value::as_object_mut) else {
        return;
    };
    let has_effort = cand
        .get("effort")
        .and_then(Value::as_str)
        .map(str::trim)
        .is_some_and(|s| !s.is_empty());
    if has_effort {
        return;
    }
    let harness = cand.get("harness").and_then(Value::as_str).unwrap_or("");
    let valid = payload
        .get("effort_ok")
        .and_then(|t| t.get(harness))
        .and_then(|m| m.get(node_effort))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if valid {
        cand.insert("effort".into(), json!(node_effort));
        if let Some(chain) = obj.get_mut("chain").and_then(Value::as_array_mut) {
            chain.push(json!(format!("node-pin effort({node_effort})")));
        }
    }
}

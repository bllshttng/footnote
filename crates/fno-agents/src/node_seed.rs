//! The node's verb against the payload's verb, decided in one place: a
//! `--node` spawn runs the verb the node derives, or refuses. The Python
//! seam projects the lifecycle table's answer (never a copy of it), the
//! seed's slot facts and the crown/resume flags; this module answers
//! pass / profile / compose / refuse, and the seam applies the answer
//! verbatim before any lane is chosen. Pure over its input: no config,
//! filesystem or network reads.

use serde_json::{json, Value};

use crate::provider::parse_verb_token;

/// The node's verb as one canonical `/word`, reusing the seed parser so no
/// second spelling rule exists. An unparseable stored verb is compared as-is.
fn canonical(node_verb: &str) -> String {
    match parse_verb_token(node_verb.trim()) {
        Some((word, _)) => format!("/{word}"),
        None => node_verb.trim().to_string(),
    }
}

/// The seed text at `argv[seed_index]`, with the `--message=` prefix
/// stripped for the `message_eq` form. `None` when there is no slot.
fn seed_text(payload: &Value) -> Option<String> {
    let index = payload.get("seed_index")?.as_u64()? as usize;
    let argv = payload.get("argv")?.as_array()?;
    let raw = argv.get(index)?.as_str()?.to_string();
    if payload.get("seed_form").and_then(Value::as_str) == Some("message_eq") {
        Some(raw.strip_prefix("--message=").unwrap_or(&raw).to_string())
    } else {
        Some(raw)
    }
}

fn argv_of(payload: &Value) -> Vec<String> {
    payload
        .get("argv")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|v| v.as_str().unwrap_or("").to_string())
                .collect()
        })
        .unwrap_or_default()
}

pub fn decide(payload: &Value) -> Value {
    let node = payload
        .get("node")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    // 1. Crown and resume spawns pass unchanged: their flags already name
    //    the work profile, and the payload carries them as facts.
    if payload.get("crown").and_then(Value::as_bool) == Some(true)
        || payload.get("resume").and_then(Value::as_bool) == Some(true)
    {
        return json!({"action": "pass"});
    }

    let seed = seed_text(payload);

    // 2. The de-stub pass spells its own command; its seed carries the token.
    if let Some(text) = &seed {
        if text.split_whitespace().any(|tok| tok == "--reconcile") {
            return json!({"action": "pass"});
        }
    }

    // 3. A seed the lifecycle family does not own passes unchecked: only a
    //    leading /target or /blueprint claims the node's verb.
    if let Some(text) = &seed {
        if let Some((first, _)) = text.split_whitespace().next().and_then(parse_verb_token) {
            let family: Vec<String> = payload
                .get("family")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            if !family.iter().any(|f| f == &format!("/{first}")) {
                return json!({"action": "pass"});
            }
        }
    }

    // 4-6. Unknown means refuse, the spawn-gate posture: a missing row, a
    //    derivation error, or a node with no verb is not evidence of
    //    /target.
    if payload.get("row_found").and_then(Value::as_bool) != Some(true) {
        return json!({"action": "refuse", "message": format!(
            "--node {node} names no readable backlog row; an unknown node is not evidence of a verb")});
    }
    if let Some(err) = payload
        .get("derive_error")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return json!({"action": "refuse", "message": format!("--node {node}: {err}")});
    }
    let raw_verb = payload
        .get("effective_verb")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            payload
                .get("stored_verb")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        });
    let node_verb = match raw_verb.map(canonical) {
        Some(v) => v,
        None => {
            return json!({"action": "refuse", "message": format!(
                "--node {node} declares no dispatch verb; encode one with fno backlog update {node} --dispatch-verb <verb>")});
        }
    };

    // 7. A family verb in the payload must agree with the node's verb.
    if let Some(text) = &seed {
        if let Some((first, _)) = text.split_whitespace().next().and_then(parse_verb_token) {
            let family: Vec<String> = payload
                .get("family")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            if family.iter().any(|f| f == &format!("/{first}")) {
                if format!("/{first}") == node_verb {
                    return json!({"action": "pass"});
                }
                return json!({"action": "refuse", "message": format!(
                    "--node {node} derives {node_verb}; the payload names /{first}. \
                     Drop the verb from the payload: --node supplies it.")});
            }
        }
    }

    let word = node_verb.trim_start_matches('/').to_string();

    // 8. No seed: the profile routes by the derived verb; the door keeps
    //    rendering the command (it also carries the brief env).
    match seed {
        Some(text) if !text.trim().is_empty() => {
            // 9. Prose: the node's command goes in front, a blank line, then
            //    the caller's text, in the slot it arrived in.
            let composed = format!("/fno:{word} {node}\n\n{text}");
            let mut argv = argv_of(payload);
            if let Some(slot) = payload.get("seed_index").and_then(Value::as_u64) {
                if let Some(tok) = argv.get_mut(slot as usize) {
                    if payload.get("seed_form").and_then(Value::as_str) == Some("message_eq")
                        && tok.starts_with("--message=")
                    {
                        *tok = format!("--message={composed}");
                    } else {
                        *tok = composed;
                    }
                }
            }
            json!({"action": "compose", "verb": word, "argv": argv})
        }
        _ => json!({"action": "profile", "verb": word}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Map};

    fn decide_map(payload: Value) -> Map<String, Value> {
        decide(&payload)
            .as_object()
            .cloned()
            .expect("decision is an object")
    }

    /// The shared shape: node x-1 derives /blueprint, no error, no crown.
    fn base() -> Value {
        json!({
            "node": "x-1", "row_found": true, "effective_verb": "/blueprint",
            "stored_verb": "/blueprint", "derive_error": null,
            "family": ["/target", "/blueprint"], "crown": false, "resume": false,
        })
    }

    // --- AC1-HP --------------------------------------------------------- //

    #[test]
    fn disagreeing_family_verb_refuses_naming_both() {
        let mut p = base().clone();
        p["argv"] = json!(["spawn", "w", "/fno:target x-1", "--node", "x-1"]);
        p["seed_index"] = json!(2);
        p["seed_form"] = json!("positional");
        let out = decide_map(p);
        assert_eq!(out["action"], "refuse");
        let msg = out["message"].as_str().unwrap();
        assert!(msg.contains("/target"), "{msg}");
        assert!(msg.contains("/blueprint"), "{msg}");
    }

    #[test]
    fn agreeing_dollar_verb_passes() {
        let mut p = base().clone();
        p["argv"] = json!(["spawn", "w", "$fno:blueprint x-1", "--node", "x-1"]);
        p["seed_index"] = json!(2);
        p["seed_form"] = json!("positional");
        assert_eq!(decide_map(p)["action"], "pass");
    }

    #[test]
    fn empty_seed_answers_profile_with_the_bare_verb() {
        let mut p = base().clone();
        p["argv"] = json!(["spawn", "w", "--node", "x-1", "--substrate", "thread"]);
        p["seed_index"] = Value::Null;
        p["seed_form"] = Value::Null;
        let out = decide_map(p);
        assert_eq!(out["action"], "profile");
        assert_eq!(out["verb"], "blueprint");
    }

    #[test]
    fn prose_seed_is_composed_at_its_slot() {
        let mut p = base().clone();
        p["argv"] = json!(["spawn", "w", "port it", "--node", "x-1"]);
        p["seed_index"] = json!(2);
        p["seed_form"] = json!("positional");
        let out = decide_map(p);
        assert_eq!(out["action"], "compose");
        assert_eq!(out["verb"], "blueprint");
        assert_eq!(
            out["argv"],
            json!([
                "spawn",
                "w",
                "/fno:blueprint x-1\n\nport it",
                "--node",
                "x-1"
            ])
        );
    }

    // --- AC2-EDGE ------------------------------------------------------- //

    #[test]
    fn out_of_family_seed_passes_unchecked() {
        let mut p = base().clone();
        p["argv"] = json!(["spawn", "w", "/fno:pr merged x-1", "--node", "x-1"]);
        p["seed_index"] = json!(2);
        p["seed_form"] = json!("positional");
        assert_eq!(decide_map(p)["action"], "pass");
    }

    #[test]
    fn reconcile_seed_passes_unchecked() {
        let mut p = base().clone();
        p["argv"] = json!([
            "spawn",
            "w",
            "$fno:target --reconcile x-1 --node x-1",
            "--node",
            "x-1"
        ]);
        p["seed_index"] = json!(2);
        p["seed_form"] = json!("positional");
        assert_eq!(decide_map(p)["action"], "pass");
    }

    #[test]
    fn crown_and_resume_spawns_pass_unchanged() {
        let mut p = base().clone();
        p["crown"] = json!(true);
        assert_eq!(decide_map(p.clone())["action"], "pass");
        p["crown"] = json!(false);
        p["resume"] = json!(true);
        assert_eq!(decide_map(p)["action"], "pass");
    }

    #[test]
    fn unknown_node_refuses() {
        let mut p = base().clone();
        p["row_found"] = json!(false);
        let out = decide_map(p);
        assert_eq!(out["action"], "refuse");
        assert!(out["message"].as_str().unwrap().contains("x-1"));
    }

    #[test]
    fn derive_error_refuses_with_the_error_text() {
        let mut p = base().clone();
        p["derive_error"] = json!("difficulty missing");
        let out = decide_map(p);
        assert_eq!(out["action"], "refuse");
        assert!(out["message"]
            .as_str()
            .unwrap()
            .contains("difficulty missing"));
    }

    #[test]
    fn verbless_node_refuses_with_the_encode_hint() {
        let mut p = base().clone();
        p["effective_verb"] = Value::Null;
        p["stored_verb"] = Value::Null;
        p["argv"] = json!(["spawn", "w", "--node", "x-1"]);
        p["seed_index"] = Value::Null;
        let out = decide_map(p);
        assert_eq!(out["action"], "refuse");
        assert!(out["message"]
            .as_str()
            .unwrap()
            .contains("fno backlog update x-1 --dispatch-verb"));
    }

    #[test]
    fn path_seed_is_prose_and_composes() {
        let mut p = base().clone();
        p["argv"] = json!(["spawn", "w", "/usr/bin/script x", "--node", "x-1"]);
        p["seed_index"] = json!(2);
        p["seed_form"] = json!("positional");
        let out = decide_map(p);
        assert_eq!(out["action"], "compose");
        assert_eq!(out["argv"][2], "/fno:blueprint x-1\n\n/usr/bin/script x");
    }

    #[test]
    fn message_eq_slot_is_rewritten_with_its_prefix() {
        let mut p = base().clone();
        p["argv"] = json!(["spawn", "w", "--message=port it", "--node", "x-1"]);
        p["seed_index"] = json!(2);
        p["seed_form"] = json!("message_eq");
        let out = decide_map(p);
        assert_eq!(out["action"], "compose");
        assert_eq!(out["argv"][2], "--message=/fno:blueprint x-1\n\nport it");
    }
}

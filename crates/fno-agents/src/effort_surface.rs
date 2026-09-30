//! The one owner of the `--effort` surface and the substrate-compatibility
//! vocabulary, ported from Python (`fno.agents.mux_spawn.effort_tokens` and
//! `fno.agents.spawn_defaults._substrate_compatible`). Python keeps bridges:
//! the refusal strings here are printed
//! verbatim, so an edit here is the one edit. Reached as payload kind
//! `compat` on the spawn-overlay verb; `bin/client.rs` asks it on the
//! thread/headless lanes.

use serde_json::{json, Map, Value};

/// Whether `harness` carries a measured capability row (Python
/// `harness_map.is_declared`): a TABLE fact, never a name list.
pub(crate) fn is_declared(harness: &str) -> bool {
    crate::harness_capabilities::HarnessContract::packaged()
        .map(|contract| contract.capabilities(harness).is_ok())
        .unwrap_or(false)
}

/// The `features.spawn` state claim for `harness` (`harness_map.spawn_state`):
/// `native`, `capable`, `absent`, or `unmeasured` when the row carries no
/// features stanza. Fail-closed, never a guess.
pub(crate) fn spawn_state(harness: &str) -> String {
    let packaged = crate::harness_capabilities::HarnessContract::packaged().ok();
    let caps = packaged.as_ref().and_then(|c| c.capabilities(harness).ok());
    caps.and_then(|caps| caps.features.get("spawn").cloned())
        .map(|claim| claim.state)
        .unwrap_or_else(|| "unmeasured".to_string())
}

/// Translate effort without maintaining a provider/model value catalog.
/// Keyed by HARNESS, not vendor: every branch is a CLI binary and the flag
/// spelling that has to be translated is the binary's. The refusal strings
/// are Python-verbatim (the Python bridge raises them; receipts quote them).
pub fn effort_tokens(harness: &str, value: &str) -> Result<Vec<String>, String> {
    if value.is_empty() {
        return Err("--effort requires a value".to_string());
    }
    if harness == "gemini" {
        return Err(format!(
            "harness {} has no reasoning-effort surface; omit --effort",
            crate::spawn_axes::repr(harness)
        ));
    }
    if harness == "claude" || harness == "agy" {
        return Ok(vec!["--effort".into(), value.to_string()]);
    }
    if harness == "codex" {
        return Ok(vec!["-c".into(), format!("model_reasoning_effort={value}")]);
    }
    if harness == "opencode" {
        return Ok(Vec::new());
    }
    if harness == "pi" {
        // pi's effort axis is `--thinking <level>`, a first-class flag: exact
        // passthrough - pi validates the vocabulary itself.
        return Ok(vec!["--thinking".into(), value.to_string()]);
    }
    if harness == "grok" {
        // grok's first-class effort flag, exact passthrough (launched with
        // `--reasoning-effort high` against 1.0.13 in the measurement).
        return Ok(vec!["--reasoning-effort".into(), value.to_string()]);
    }
    if harness == "cursor-agent" {
        return Err(
            "harness 'cursor-agent' has no --effort flag; effort is encoded in \
             the selected --model value"
                .to_string(),
        );
    }
    if !is_declared(harness) {
        // The CLI seam validates --effort before routing, so an undeclared
        // harness reaches THIS refusal first. The advice names the undeclared
        // lane's own escape (the operator's `--` passthrough), not "omit the
        // flag" - the vendor's spelling is real, just unmeasured.
        return Err(format!(
            "--effort is not available for harness {}: fno has no \
             capability row for it, so there is no measured mapping to the \
             vendor's own flag spelling. Pass the vendor's own flag after \
             '--' instead.",
            crate::spawn_axes::repr(harness)
        ));
    }
    Err(format!(
        "harness {} has no reasoning-effort surface; omit --effort",
        crate::spawn_axes::repr(harness)
    ))
}

/// A config-sourced substrate must be a KNOWN value AND honored by the
/// resolved harness. `thread` requires the harness's journey-proven fno
/// driver (its spawn claim reads native); `pane`/`headless` are universal.
/// `bg` reads as the deprecated alias for `thread`. An unknown value (or
/// `thread` on a non-thread harness) answers false - the caller warns and
/// skips, never injects to fail at the spawn parser. The Python bridge keeps
/// `harness == "claude"` for an unavailable owner; this owner is never
/// unavailable in-process.
pub fn substrate_compatible(substrate: &str, harness: &str) -> bool {
    const SUBSTRATES: [&str; 4] = ["pane", "thread", "headless", "bg"];
    if !SUBSTRATES.contains(&substrate) {
        return false;
    }
    let substrate = if substrate == "bg" {
        "thread"
    } else {
        substrate
    };
    if substrate != "thread" {
        return true;
    }
    // Python's bridge arm for an unreadable table answers claude-only; the
    // same fallback covers a harness the contract does not declare.
    if !is_declared(harness) {
        return harness == "claude";
    }
    spawn_state(harness) == "native"
}

/// The `kind: "compat"` answer: one owner for the effort surface and the
/// substrate vocabulary, each field optional. `effort` answers
/// `{"tokens": [...]}` or `{"refusal": "..."}`; `substrate` answers
/// `{"compatible": bool}`.
pub fn compat(payload: &Value) -> Value {
    let harness = payload.get("harness").and_then(Value::as_str).unwrap_or("");
    let mut out = Map::new();
    if let Some(value) = payload.get("effort").and_then(Value::as_str) {
        let entry = match effort_tokens(harness, value) {
            Ok(tokens) => json!({"tokens": tokens}),
            Err(refusal) => json!({"refusal": refusal}),
        };
        out.insert("effort".into(), entry);
    }
    if let Some(substrate) = payload.get("substrate").and_then(Value::as_str) {
        out.insert(
            "substrate".into(),
            json!({"compatible": substrate_compatible(substrate, harness)}),
        );
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- AC4-HP: the per-harness spellings ------------------------------ //

    #[test]
    fn effort_spellings_match_the_measured_table() {
        assert_eq!(
            effort_tokens("claude", "high").unwrap(),
            vec!["--effort".to_string(), "high".to_string()]
        );
        assert_eq!(
            effort_tokens("codex", "high").unwrap(),
            vec!["-c".to_string(), "model_reasoning_effort=high".to_string()]
        );
        assert_eq!(
            effort_tokens("opencode", "high").unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            effort_tokens("pi", "high").unwrap(),
            vec!["--thinking".to_string(), "high".to_string()]
        );
        assert_eq!(
            effort_tokens("grok", "high").unwrap(),
            vec!["--reasoning-effort".to_string(), "high".to_string()]
        );
        assert_eq!(
            effort_tokens("agy", "high").unwrap(),
            vec!["--effort".to_string(), "high".to_string()]
        );
    }

    // --- AC4-ERR: the deny set ------------------------------------------ //

    #[test]
    fn effort_denies_with_the_python_strings() {
        assert_eq!(
            effort_tokens("gemini", "high"),
            Err("harness 'gemini' has no reasoning-effort surface; omit --effort".to_string())
        );
        assert_eq!(
            effort_tokens("cursor-agent", "high"),
            Err(
                "harness 'cursor-agent' has no --effort flag; effort is encoded in \
                 the selected --model value"
                    .to_string()
            )
        );
        assert_eq!(
            effort_tokens("claude", ""),
            Err("--effort requires a value".to_string())
        );
        let undeclared = effort_tokens("ghosth", "high");
        assert!(undeclared.unwrap_err().starts_with(
            "--effort is not available for harness 'ghosth': fno has no capability row"
        ));
    }

    // --- AC5-HP: the substrate table ------------------------------------- //

    #[test]
    fn thread_reads_the_spawn_claim_per_harness() {
        for harness in [
            "claude",
            "codex",
            "opencode",
            "pi",
            "agy",
            "grok",
            "cursor-agent",
        ] {
            assert!(
                substrate_compatible("thread", harness),
                "thread seatable: {harness}"
            );
        }
        assert!(!substrate_compatible("thread", "gemini"));
    }

    #[test]
    fn bg_reads_as_thread_and_pane_headless_are_universal() {
        for harness in ["claude", "codex", "gemini", "ghosth"] {
            assert!(substrate_compatible("bg", harness) == substrate_compatible("thread", harness));
            assert!(substrate_compatible("pane", harness));
            assert!(substrate_compatible("headless", harness));
        }
        assert!(!substrate_compatible("wat", "claude"));
    }

    #[test]
    fn compat_answers_both_fields() {
        let out = compat(
            &json!({"kind": "compat", "harness": "codex", "effort": "high", "substrate": "thread"}),
        );
        assert_eq!(
            out["effort"]["tokens"],
            json!(["-c", "model_reasoning_effort=high"])
        );
        assert_eq!(out["substrate"]["compatible"], json!(true));
        let refused = compat(&json!({"kind": "compat", "harness": "gemini", "effort": "high"}));
        assert_eq!(
            refused["effort"]["refusal"],
            json!("harness 'gemini' has no reasoning-effort surface; omit --effort")
        );
        let none = compat(&json!({"kind": "compat", "harness": "claude"}));
        assert!(none.get("effort").is_none());
        assert!(none.get("substrate").is_none());
    }
}

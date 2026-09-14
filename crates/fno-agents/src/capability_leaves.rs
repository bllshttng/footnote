//! The two read leaves the dispatch-verb retirement left behind (x-3873
//! change 2): `capabilities` reads the packaged harness capability table and
//! `target-family` classifies a message against the merge-posture family
//! table. Both are hidden Rust-only `fno agents` verbs that answer a question
//! scripts ask; they direct-dispatch in client.rs and never touch the daemon.

use crate::merge_posture::is_target_family;
use serde_json::Value;

/// `capabilities <harness> [--json|-J]`: one harness's config-independent
/// capability contract, read straight from the packaged table. The successor
/// to `fno agents dispatch capabilities` (x-3873): the JSON shape
/// (map_version, harness, then the harness's table) matches what the Python
/// leaf printed. An unknown harness exits 2 naming the harness and the
/// declared list, with nothing on stdout.
pub fn run_capabilities(args: &[String]) -> i32 {
    let json = args.iter().any(|a| a == "--json" || a == "-J");
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    if positional.len() != 1 {
        eprintln!("fno agents capabilities: exactly one harness argument is required");
        return 2;
    }
    let harness = positional[0].as_str();
    let (code, payload) = match capabilities_json(harness) {
        Ok(payload) => (0, payload),
        Err(message) => {
            eprintln!("{message}");
            return 2;
        }
    };
    let text = if json {
        serde_json::to_string(&payload).expect("serialized JSON")
    } else {
        serde_json::to_string_pretty(&payload).expect("serialized JSON")
    };
    println!("{text}");
    code
}

/// The capabilities answer, split from the printer so tests can read it.
/// `Err` carries the stderr line (the harness named, plus the declared list).
fn capabilities_json(harness: &str) -> Result<Value, String> {
    let contract = crate::harness_capabilities::HarnessContract::packaged()
        .map_err(|e| format!("fno agents capabilities: capability contract error: {e}"))?;
    let caps = contract.capabilities(harness).map_err(|_| {
        let declared: Vec<&str> = contract.harness.keys().map(String::as_str).collect();
        format!(
            "fno agents capabilities: unknown harness {harness:?}; the \
             packaged capability contract knows: {}",
            declared.join(", ")
        )
    })?;
    let caps_value = serde_json::to_value(caps)
        .map_err(|e| format!("fno agents capabilities: serialization error: {e}"))?;
    // Header fields first, then the harness table flattened beside them: the
    // key order the Python leaf printed (`{"map_version": ..., "harness":
    // ..., **caps_table}`).
    let mut out = serde_json::Map::new();
    out.insert(
        "map_version".to_string(),
        Value::Number(serde_json::Number::from(contract.map_version)),
    );
    out.insert("harness".to_string(), Value::String(harness.to_string()));
    if let Value::Object(table) = caps_value {
        for (key, value) in table {
            out.insert(key, value);
        }
    }
    Ok(Value::Object(out))
}

/// `target-family --message <m>`: print `family` when the message's first
/// token is a /target-family spelling, `other` otherwise, exit 0 either way.
/// The successor to `fno agents dispatch family` (x-3873); the merge_posture
/// table is the same one the Rust merge-posture reads use.
pub fn run_target_family(args: &[String]) -> i32 {
    let mut message: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if let Some(v) = arg.strip_prefix("--message=") {
            if message.is_some() {
                eprintln!("fno agents target-family: --message given twice");
                return 2;
            }
            message = Some(v.to_string());
            i += 1;
        } else if arg == "--message" || arg == "-m" {
            if message.is_some() || i + 1 >= args.len() {
                eprintln!("fno agents target-family: --message takes exactly one value");
                return 2;
            }
            message = Some(args[i + 1].clone());
            i += 2;
        } else {
            eprintln!("fno agents target-family: unknown argument {arg:?}");
            return 2;
        }
    }
    let Some(message) = message else {
        eprintln!("fno agents target-family: --message is required");
        return 2;
    };
    let verdict = if is_target_family(&message) {
        "family"
    } else {
        "other"
    };
    println!("{verdict}");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_json_carries_the_header_and_the_harness_table() {
        let out = capabilities_json("codex").expect("codex is a declared harness");
        assert_eq!(out["harness"], "codex");
        assert!(out["map_version"].is_u64());
        // AC2-HP: the four fields the agent scripts read survive the port.
        assert_eq!(out["command_surface"], "codex-skill");
        assert!(out["keeper"].is_null());
        assert_eq!(
            out["resume_strategy"]["forms"]["interactive_attach"]["tokens"],
            serde_json::json!(["codex", "resume", "{session_id}", "--remote", "unix://"])
        );
    }

    #[test]
    fn capabilities_unknown_harness_names_the_name_and_the_declared_list() {
        let err = capabilities_json("nosuch").unwrap_err();
        assert!(err.contains("nosuch"));
        assert!(err.contains("claude"));
        assert!(err.contains("knows:"));
    }

    #[test]
    fn target_family_answers_family_then_other_and_always_exits_zero() {
        // AC3-HP: a /target-family message reads family; prose reads other.
        let family = run_target_family(&["--message".to_string(), "/fno:target x-1".to_string()]);
        assert_eq!(family, 0);
        let other = run_target_family(&["--message".to_string(), "hello".to_string()]);
        assert_eq!(other, 0);
        assert!(is_target_family("/fno:target x-1"));
        assert!(!is_target_family("hello"));
    }

    #[test]
    fn target_family_refuses_a_missing_message() {
        assert_eq!(run_target_family(&[]), 2);
        assert_eq!(run_target_family(&["--message".to_string()]), 2);
    }
}

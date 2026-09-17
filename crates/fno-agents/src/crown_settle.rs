//! `crown-settle`: whether a crowned spawn is granted, transfers, or refuses,
//! reached as payload kind `crown-settle` on the existing `spawn-overlay`
//! verb (law d-fe66560a bars new top-level verbs, hidden ones included).
//!
//! A port of Python's `settle_spawn_crown` (`cli/src/fno/agents/crown.py`)
//! with one new branch: a human caller with `succession` may transfer a
//! crown away from any live holder, the same authority `grant_error` already
//! gives a human to bestow any scope (a human may grant what nobody above it
//! could check). An agent caller may only succeed itself: every live holder
//! of the scope must already be that agent.
//!
//! Rows arrive as plain JSON here, not a typed registry row, so a row's
//! status is matched by string against Python's `TERMINAL_STATUSES`
//! (`registry.py:105`) exactly, the same string-match reasoning
//! `loop_reign.rs` documents for its own terminal check.

use serde_json::{json, Value};

const TERMINAL_STATUSES: [&str; 4] = ["exited", "orphaned", "failed", "permanent_dead"];

enum Caller {
    Human,
    Agent(String),
}

fn parse_caller(value: Option<&Value>) -> Option<Caller> {
    match value?.get("kind").and_then(Value::as_str)? {
        "human" => Some(Caller::Human),
        "agent" => Some(Caller::Agent(
            value?.get("name").and_then(Value::as_str)?.to_string(),
        )),
        _ => None,
    }
}

/// Decide occupancy for one crowned spawn over `scope`. See the module doc
/// for the caller/succession rules; the request and answer shapes are
/// documented on the plan this ports (fno x-3f1c).
pub fn resolve(payload: &Value) -> Result<Value, String> {
    let scope = payload
        .get("scope")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "crown-settle: payload needs a non-empty scope".to_string())?;
    let rows = payload
        .get("rows")
        .and_then(Value::as_array)
        .ok_or_else(|| "crown-settle: payload needs a rows array".to_string())?;
    let caller = parse_caller(payload.get("caller"))
        .ok_or_else(|| "crown-settle: payload needs a caller of kind human or agent".to_string())?;
    let succession = payload
        .get("succession")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let exclude_name = payload.get("exclude_name").and_then(Value::as_str);

    let mut clear_terminal = Vec::new();
    let mut holders = Vec::new();
    for row in rows {
        let row_scope = row.get("crown_scope").and_then(Value::as_str);
        if row_scope != Some(scope) {
            continue;
        }
        let name = row.get("name").and_then(Value::as_str).unwrap_or("");
        let status = row.get("status").and_then(Value::as_str).unwrap_or("");
        if TERMINAL_STATUSES.contains(&status) {
            clear_terminal.push(name.to_string());
            continue;
        }
        if Some(name) == exclude_name {
            continue;
        }
        holders.push(name.to_string());
    }
    holders.sort();

    if holders.is_empty() {
        return Ok(json!({
            "outcome": "granted",
            "clear_terminal": clear_terminal,
            "holders": holders,
            "vacate": Vec::<String>::new(),
            "refusal": Value::Null,
        }));
    }

    if succession {
        match &caller {
            Caller::Agent(name) if holders.iter().all(|h| h == name) => {
                return Ok(json!({
                    "outcome": "succeeded",
                    "clear_terminal": clear_terminal,
                    "holders": holders,
                    "vacate": [name.clone()],
                    "refusal": Value::Null,
                }));
            }
            Caller::Human => {
                return Ok(json!({
                    "outcome": "succeeded",
                    "clear_terminal": clear_terminal,
                    "vacate": holders.clone(),
                    "holders": holders,
                    "refusal": Value::Null,
                }));
            }
            Caller::Agent(_) => {}
        }
    }

    let refusal = match &caller {
        Caller::Human => format!(
            "scope {scope:?} is held by live row(s) {holders:?}. This spawn would launch \
             an heir with no crown, so it refuses. Re-run with --succeed to transfer the \
             crown to the new session, or choose a scope nobody holds."
        ),
        Caller::Agent(_) => format!(
            "scope {scope:?} is held by live row(s) {holders:?}, not by this session, so \
             this session cannot hand it down. Only the holder (spawn --crown --succeed \
             from its own session) or an attended shell (spawn --crown --succeed) can \
             transfer it."
        ),
    };

    Ok(json!({
        "outcome": "declined",
        "clear_terminal": clear_terminal,
        "holders": holders,
        "vacate": Vec::<String>::new(),
        "refusal": refusal,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(name: &str, scope: &str, status: &str) -> Value {
        json!({"name": name, "crown_scope": scope, "status": status})
    }

    #[test]
    fn no_live_holder_grants_for_any_caller() {
        let out = resolve(&json!({
            "kind": "crown-settle", "scope": "fno", "succession": false,
            "caller": {"kind": "human"}, "rows": [],
        }))
        .unwrap();
        assert_eq!(out["outcome"], "granted");
        assert_eq!(out["vacate"], json!([]));
        assert!(out["refusal"].is_null());
    }

    #[test]
    fn agent_succession_over_its_own_crown_succeeds() {
        let out = resolve(&json!({
            "kind": "crown-settle", "scope": "fno", "succession": true,
            "caller": {"kind": "agent", "name": "king-a"},
            "rows": [row("king-a", "fno", "busy")],
        }))
        .unwrap();
        assert_eq!(out["outcome"], "succeeded");
        assert_eq!(out["vacate"], json!(["king-a"]));
    }

    #[test]
    fn human_succession_transfers_a_live_holder() {
        let out = resolve(&json!({
            "kind": "crown-settle", "scope": "fno", "succession": true,
            "caller": {"kind": "human"},
            "rows": [row("king-a", "fno", "busy")],
        }))
        .unwrap();
        assert_eq!(out["outcome"], "succeeded");
        assert_eq!(out["vacate"], json!(["king-a"]));
    }

    #[test]
    fn terminal_holder_clears_and_grants() {
        let out = resolve(&json!({
            "kind": "crown-settle", "scope": "fno", "succession": false,
            "caller": {"kind": "human"},
            "rows": [row("dead-king", "fno", "exited")],
        }))
        .unwrap();
        assert_eq!(out["clear_terminal"], json!(["dead-king"]));
        assert_eq!(out["outcome"], "granted");
    }

    #[test]
    fn human_without_succession_declines_naming_holder_and_flag() {
        let out = resolve(&json!({
            "kind": "crown-settle", "scope": "fno", "succession": false,
            "caller": {"kind": "human"},
            "rows": [row("king-a", "fno", "busy")],
        }))
        .unwrap();
        assert_eq!(out["outcome"], "declined");
        let refusal = out["refusal"].as_str().unwrap();
        assert!(refusal.contains("king-a"));
        assert!(refusal.contains("--succeed"));
    }

    #[test]
    fn agent_over_a_scope_it_does_not_hold_declines() {
        let out = resolve(&json!({
            "kind": "crown-settle", "scope": "fno", "succession": false,
            "caller": {"kind": "agent", "name": "other"},
            "rows": [row("king-a", "fno", "busy")],
        }))
        .unwrap();
        assert_eq!(out["outcome"], "declined");
        let refusal = out["refusal"].as_str().unwrap();
        assert!(refusal.contains("king-a"));
        assert!(refusal.contains("--succeed"));
    }

    #[test]
    fn revive_excludes_its_own_name_from_holders() {
        let out = resolve(&json!({
            "kind": "crown-settle", "scope": "fno", "succession": false,
            "caller": {"kind": "human"}, "exclude_name": "heir",
            "rows": [row("heir", "fno", "busy")],
        }))
        .unwrap();
        assert_eq!(out["holders"], json!([]));
        assert_eq!(out["outcome"], "granted");
    }

    #[test]
    fn missing_scope_is_an_error() {
        assert!(resolve(&json!({
            "kind": "crown-settle", "caller": {"kind": "human"}, "rows": [],
        }))
        .is_err());
    }
}

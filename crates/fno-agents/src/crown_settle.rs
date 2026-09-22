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
//! status is matched by string against `announce::TERMINAL_STATUSES`
//! (itself `registry.py::TERMINAL_STATUSES`), the same string-match
//! reasoning `announce.rs` documents for its own terminal check.
//!
//! Occupancy is the exact-scope holders plus ladder-aware rivals: through
//! `loop_king::crown_rivals`, a live crown over overlapping territory
//! declines the spawn before the succession branch, and an agent's own
//! wider crown is its delegation, not a rival.

use crate::announce::TERMINAL_STATUSES;
use crate::loop_king::{crown_rivals_pub, same_territory, scopes_overlap};
use serde_json::{json, Value};
use std::collections::HashMap;

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
/// for the caller/succession rules and the request/answer shapes.
pub fn resolve(payload: &Value) -> Result<Value, String> {
    let projects = crate::king_board::project_map(&std::env::current_dir().unwrap_or_default());
    resolve_with_projects(payload, &projects)
}

fn resolve_with_projects(
    payload: &Value,
    projects: &Result<HashMap<String, String>, String>,
) -> Result<Value, String> {
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

    let empty_map: HashMap<String, String> = HashMap::new();
    let map = projects.as_ref().unwrap_or(&empty_map);
    let mut rivals: Vec<(String, String)> = Vec::new();
    for row in rows {
        let Some(held) = row.get("crown_scope").and_then(Value::as_str) else {
            continue;
        };
        if held.trim().is_empty() || held == scope {
            continue;
        }
        let name = row.get("name").and_then(Value::as_str).unwrap_or("");
        let status = row.get("status").and_then(Value::as_str).unwrap_or("");
        if TERMINAL_STATUSES.contains(&status) || Some(name) == exclude_name {
            continue;
        }
        // An agent delegating one member of its own crown does not rival
        // itself; grant_error verified strict containment before crown-settle
        // runs, and promote_existing_session skips the grantor the same way.
        if let Caller::Agent(me) = &caller {
            if name == me && !same_territory(held, scope, map) {
                continue;
            }
        }
        let rival = if projects.is_ok() {
            crown_rivals_pub(held, None, scope, None, map)
        } else {
            scopes_overlap(held, scope, &empty_map)
        };
        if rival {
            rivals.push((name.to_string(), held.to_string()));
        }
    }
    rivals.sort();

    if let Some((first, _)) = rivals.first() {
        let listed = rivals
            .iter()
            .map(|(n, h)| format!("{n} (holding {h:?})"))
            .collect::<Vec<_>>()
            .join(", ");
        let refusal = format!(
            "scope {scope:?} overlaps territory held by live row(s) {listed}. Two live \
             crowns would rule the same members, so this spawn refuses before launch. \
             --succeed hands down only an identical crown, never part of a wider or \
             overlapping one. Re-scope the holder (fno agents crown {first} --scope \
             <other territory>), run fno agents reconcile if it looks dead, or fno \
             agents stop {first}, then retry."
        );
        return Ok(json!({
            "outcome": "declined",
            "clear_terminal": clear_terminal,
            "holders": holders,
            "vacate": Vec::<String>::new(),
            "rivals": rivals.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
            "refusal": refusal,
        }));
    }

    if holders.is_empty() {
        return Ok(json!({
            "outcome": "granted",
            "clear_terminal": clear_terminal,
            "holders": holders,
            "vacate": Vec::<String>::new(),
            "rivals": Vec::<String>::new(),
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
                    "rivals": Vec::<String>::new(),
                    "refusal": Value::Null,
                }));
            }
            Caller::Human => {
                return Ok(json!({
                    "outcome": "succeeded",
                    "clear_terminal": clear_terminal,
                    "vacate": holders.clone(),
                    "holders": holders,
                    "rivals": Vec::<String>::new(),
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
        "rivals": Vec::<String>::new(),
        "refusal": refusal,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

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
    fn agent_succession_over_a_multi_holder_scope_declines() {
        // holders = ["king-a", "king-b"]; the caller matches one but not all,
        // so succession must fall through to the ordinary decline rather
        // than succeeding a partial match.
        let out = resolve(&json!({
            "kind": "crown-settle", "scope": "fno", "succession": true,
            "caller": {"kind": "agent", "name": "king-a"},
            "rows": [row("king-a", "fno", "busy"), row("king-b", "fno", "busy")],
        }))
        .unwrap();
        assert_eq!(out["outcome"], "declined");
        assert_eq!(out["vacate"], json!([]));
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

    // -- rivals: occupancy beyond the exact scope string, map injected --

    fn settle(payload: Value, projects: Result<HashMap<String, String>, String>) -> Value {
        resolve_with_projects(&payload, &projects).unwrap()
    }

    #[test]
    fn a_member_of_a_live_set_crown_declines_naming_the_rival() {
        let out = settle(
            json!({
                "kind": "crown-settle", "scope": "epic-a", "succession": false,
                "caller": {"kind": "human"},
                "rows": [row("king-a", "epic-a,epic-b", "busy")],
            }),
            Ok(HashMap::new()),
        );
        assert_eq!(out["outcome"], "declined");
        assert_eq!(out["rivals"], json!(["king-a"]));
        assert_eq!(out["vacate"], json!([]));
        let refusal = out["refusal"].as_str().unwrap();
        assert!(refusal.contains("king-a"));
        assert!(refusal.contains("epic-a,epic-b"));
    }

    #[test]
    fn a_set_scope_over_one_live_member_declines_for_any_caller() {
        let rows = [row("king-a", "epic-a", "busy")];
        for caller in [
            json!({"kind": "human"}),
            json!({"kind": "agent", "name": "other"}),
        ] {
            let out = settle(
                json!({
                    "kind": "crown-settle", "scope": "epic-a,epic-b", "succession": false,
                    "caller": caller, "rows": rows,
                }),
                Ok(HashMap::new()),
            );
            assert_eq!(out["outcome"], "declined");
            assert_eq!(out["rivals"], json!(["king-a"]));
        }
    }

    #[test]
    fn succession_does_not_transfer_a_rival_crown() {
        let out = settle(
            json!({
                "kind": "crown-settle", "scope": "epic-a", "succession": true,
                "caller": {"kind": "human"},
                "rows": [row("king-a", "epic-a,epic-b", "busy")],
            }),
            Ok(HashMap::new()),
        );
        assert_eq!(out["outcome"], "declined");
        assert_eq!(out["vacate"], json!([]));
        assert!(out["refusal"]
            .as_str()
            .unwrap()
            .contains("--succeed hands down only an identical crown"));
    }

    #[test]
    fn an_agents_own_wider_crown_is_its_delegation_not_a_rival() {
        let out = settle(
            json!({
                "kind": "crown-settle", "scope": "epic-a", "succession": false,
                "caller": {"kind": "agent", "name": "king-a"},
                "rows": [row("king-a", "epic-a,epic-b", "busy")],
            }),
            Ok(HashMap::new()),
        );
        assert_eq!(out["outcome"], "granted");
        assert_eq!(out["rivals"], json!([]));
    }

    #[test]
    fn a_portfolio_crown_over_one_of_its_projects_is_a_court_not_a_rival() {
        let projects = Ok(HashMap::from([
            ("alpha".to_string(), "alpha".to_string()),
            ("beta".to_string(), "beta".to_string()),
        ]));
        let out = settle(
            json!({
                "kind": "crown-settle", "scope": "alpha", "succession": false,
                "caller": {"kind": "human"},
                "rows": [row("king-p", "alpha,beta", "busy")],
            }),
            projects,
        );
        assert_eq!(out["outcome"], "granted");
        assert_eq!(out["rivals"], json!([]));
    }

    #[test]
    fn an_unreadable_project_map_fails_closed_to_raw_overlap() {
        let out = settle(
            json!({
                "kind": "crown-settle", "scope": "alpha", "succession": false,
                "caller": {"kind": "human"},
                "rows": [row("king-p", "alpha,beta", "busy")],
            }),
            Err("no work.workspaces in any candidate config.toml".to_string()),
        );
        assert_eq!(out["outcome"], "declined");
        assert_eq!(out["rivals"], json!(["king-p"]));
    }

    #[test]
    fn an_alias_spelled_same_territory_row_is_a_rival() {
        let projects = Ok(HashMap::from([
            ("alpha".to_string(), "alpha".to_string()),
            ("a".to_string(), "alpha".to_string()),
        ]));
        let out = settle(
            json!({
                "kind": "crown-settle", "scope": "alpha", "succession": false,
                "caller": {"kind": "human"},
                "rows": [row("king-a", "a", "busy")],
            }),
            projects,
        );
        assert_eq!(out["outcome"], "declined");
        assert_eq!(out["rivals"], json!(["king-a"]));
    }

    #[test]
    fn a_terminal_row_rivals_nothing() {
        let out = settle(
            json!({
                "kind": "crown-settle", "scope": "epic-a", "succession": false,
                "caller": {"kind": "human"},
                "rows": [row("dead-king", "epic-a,epic-b", "exited")],
            }),
            Ok(HashMap::new()),
        );
        assert_eq!(out["outcome"], "granted");
        assert_eq!(out["rivals"], json!([]));
        assert_eq!(out["clear_terminal"], json!([]));
    }
}

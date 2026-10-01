//! `team-identity`: the lock-time identity match for the team verbs,
//! reached as payload kind `team-identity` on the existing `spawn-overlay`
//! verb (law d-fe66560a bars new top-level verbs, hidden ones included).
//!
//! Three team verbs resolve a target row before the registry lock and used
//! to find that row again by NAME alone under the lock:
//! `promote_existing_session` stamps its target, `reclaim_team` finds its
//! holder, and `lead done` vacates the caller's own row. Registry names are
//! reclaimable (an exited row is dropped and a new one appended under the
//! old name), so a name rebound inside that window was teamed, reclaimed
//! or vacated in place of the session the caller resolved. The verbs now
//! carry the row's session id from the pre-lock read and this kind matches
//! name and session as a pair under the lock, the way the lead manifest
//! removal compares `expected_harness_session_id`. Rows whose session
//! fields are all absent compare by name alone.
//!
//! Fail closed: no pair match is an answer (`matched: false`), and a Python
//! caller that cannot reach this kind refuses the team action outright.

use serde_json::{json, Value};

/// The row's session identity, in the precedence every team reader uses:
/// `harness_session_id`, then `cc_session_id`, then `short_id`. All absent
/// is `None`, and only an expectation of `None` matches it.
fn row_session(row: &Value) -> Option<String> {
    for field in ["harness_session_id", "cc_session_id", "short_id"] {
        if let Some(session) = row.get(field).and_then(Value::as_str) {
            if !session.is_empty() {
                return Some(session.to_string());
            }
        }
    }
    None
}

/// Decide the lock-time match for one carried identity. `expect` names the
/// row resolved BEFORE the lock; `rows` is what the write actually sees.
pub fn resolve(payload: &Value) -> Result<Value, String> {
    let rows = payload
        .get("rows")
        .and_then(Value::as_array)
        .ok_or_else(|| "team-identity: payload needs a rows array".to_string())?;
    let expect = payload
        .get("expect")
        .ok_or_else(|| "team-identity: payload needs an expect object".to_string())?;
    let name = expect
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| "team-identity: expect needs a non-empty name".to_string())?;
    let session = match expect.get("harness_session_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(session)) => Some(session.clone()),
        Some(_) => {
            return Err("team-identity: expect session must be a string or null".to_string())
        }
    };
    for (index, row) in rows.iter().enumerate() {
        if row.get("name").and_then(Value::as_str) != Some(name) {
            continue;
        }
        if row_session(row) == session {
            return Ok(json!({"matched": true, "index": index}));
        }
    }
    Ok(json!({"matched": false}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(name: &str, session: Value) -> Value {
        json!({"name": name, "harness_session_id": session})
    }

    #[test]
    fn the_carried_pair_matches_its_row() {
        let out = resolve(&json!({
            "rows": [row("worker", "sess-old".into()), row("other", "sess-x".into())],
            "expect": {"name": "worker", "harness_session_id": "sess-old"},
        }))
        .unwrap();
        assert_eq!(out["matched"], true);
        assert_eq!(out["index"], 0);
    }

    #[test]
    fn a_name_rebound_to_another_session_does_not_match() {
        let out = resolve(&json!({
            "rows": [row("worker", "sess-new".into())],
            "expect": {"name": "worker", "harness_session_id": "sess-old"},
        }))
        .unwrap();
        assert_eq!(out["matched"], false);
    }

    #[test]
    fn a_sessionless_expectation_refuses_a_sessionful_row() {
        // The row resolved before the lock had no session id; the name was
        // rebound to a row that does. Name-alone matching would team it.
        let out = resolve(&json!({
            "rows": [row("worker", "sess-new".into())],
            "expect": {"name": "worker", "harness_session_id": null},
        }))
        .unwrap();
        assert_eq!(out["matched"], false);
    }

    #[test]
    fn two_sessionless_rows_still_compare_by_name_alone() {
        let out = resolve(&json!({
            "rows": [
                json!({"name": "worker"}),
                json!({"name": "ghost", "harness_session_id": null}),
            ],
            "expect": {"name": "worker", "harness_session_id": null},
        }))
        .unwrap();
        assert_eq!(out["matched"], true);
        assert_eq!(out["index"], 0);
    }

    #[test]
    fn the_session_falls_back_through_the_team_precedence() {
        let out = resolve(&json!({
            "rows": [json!({
                "name": "worker", "harness_session_id": null, "cc_session_id": "cc-1",
            })],
            "expect": {"name": "worker", "harness_session_id": "cc-1"},
        }))
        .unwrap();
        assert_eq!(out["matched"], true);
    }

    #[test]
    fn an_empty_session_field_is_absent_not_an_identity() {
        let out = resolve(&json!({
            "rows": [json!({
                "name": "worker", "harness_session_id": "", "short_id": "abcd1234",
            })],
            "expect": {"name": "worker", "harness_session_id": "abcd1234"},
        }))
        .unwrap();
        assert_eq!(out["matched"], true);
    }

    #[test]
    fn malformed_payloads_are_errors_not_answers() {
        assert!(resolve(&json!({"expect": {"name": "w"}})).is_err());
        assert!(resolve(&json!({"rows": []})).is_err());
        assert!(resolve(&json!({"rows": [], "expect": {"name": ""}})).is_err());
        assert!(
            resolve(&json!({"rows": [], "expect": {"name": "w", "harness_session_id": 7}}))
                .is_err()
        );
    }
}

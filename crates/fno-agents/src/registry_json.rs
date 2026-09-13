//! `fno-agents registry-json`: the daemon-free registry projection the hooks
//! read, with the served liveness pair layered on top of the stored
//! eight-key row. Split out of `client_verbs.rs` rather than grown there:
//! that file is over the 5,000-line budget and shrink-only.

use crate::client_verbs::{load_registry_entries, resume_session_id, to_python_json};
use crate::paths::AgentsHome;
use serde_json::{json, Value};

/// Takes no arguments (Python's Typer command took none); a registry load
/// failure maps to `trace`'s exit 12.
pub fn run_registry_json(rest: &[String], home: &AgentsHome) -> i32 {
    if let Some(arg) = rest.first() {
        eprintln!("fno-agents: registry-json takes no arguments (got: {arg})");
        return 2;
    }
    let registry_path = home.registry_json();
    let rows = match load_registry_entries(&registry_path) {
        Ok(rows) => rows,
        Err(exc) => {
            eprintln!("fno agents registry-json: registry load failed: {exc}");
            return 12;
        }
    };
    println!("{}", to_python_json(&registry_json_logic(&rows)));
    0
}

/// The pure registry-json projection (no I/O); [`run_registry_json`] writes
/// its output. One row per registry row: the eight stored keys Python's
/// `cmd_registry_json` emitted, plus the two SERVED liveness fields derived
/// here by the vendored freshness rule (`served_liveness.rs`), never passed
/// through raw -- past the window the word is withheld and the basis says
/// why. A stored row's own `liveness` field is the measurement INPUT, not
/// the served word; both served keys shadow it. Keys serialize in the
/// `json!{}` macro's literal field order (`preserve_order` is on for this
/// crate); consumers read by key, never by position.
pub(crate) fn registry_json_logic(rows: &[Value]) -> Value {
    let agents: Vec<Value> = rows
        .iter()
        .map(|row| {
            let harness = row.get("harness").and_then(Value::as_str).unwrap_or("");
            let session_id = resume_session_id(row, harness);
            let word = row.get("liveness").and_then(Value::as_str);
            let measured_at = row.get("liveness_measured_at").and_then(Value::as_str);
            json!({
                "name": row.get("name").and_then(Value::as_str),
                "session_id": if session_id.is_empty() {
                    Value::Null
                } else {
                    Value::String(session_id.to_string())
                },
                "harness_session_id": row.get("harness_session_id").and_then(Value::as_str),
                "status": row.get("status").and_then(Value::as_str).unwrap_or("live"),
                "crown_level": row.get("crown_level").cloned().unwrap_or(Value::Null),
                "crown_scope": row.get("crown_scope").and_then(Value::as_str),
                "spawned_by_session": row.get("spawned_by_session").and_then(Value::as_str),
                "origin": row.get("origin").and_then(Value::as_str),
                "liveness": crate::row_truth::served_fresh_liveness(word, measured_at),
                "liveness_basis": crate::row_truth::served_liveness_basis(word, measured_at),
            })
        })
        .collect();
    json!({ "agents": agents })
}

#[cfg(test)]
mod tests {
    use super::*;

    // The 20-byte Z stamp `rfc3339_like_to_secs` accepts, generated at test
    // runtime: a hardcoded stamp ages past the 120-second window and the
    // suite turns red on a clock, not on a defect.
    fn z_stamp(age_secs: i64) -> String {
        (chrono::Utc::now() - chrono::Duration::seconds(age_secs))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    }

    fn row(fields: Value) -> Value {
        let mut base = json!({
            "name": "w1", "harness": "codex", "status": "live",
            "crown_level": null, "crown_scope": null,
            "spawned_by_session": "parent-1", "origin": null,
        });
        let obj = base.as_object_mut().unwrap();
        for (k, v) in fields.as_object().unwrap() {
            obj.insert(k.clone(), v.clone());
        }
        base
    }

    #[test]
    fn fresh_stamps_serve_the_stored_word_and_their_basis() {
        let out = registry_json_logic(&[row(json!({
            "harness_session_id": "s-1",
            "liveness": "dead", "liveness_measured_at": z_stamp(10),
        }))]);
        let a = &out["agents"][0];
        assert_eq!(a["liveness"], "dead", "a fresh measurement serves its word");
        assert_eq!(a["liveness_basis"], "fresh");
    }

    #[test]
    fn stale_stamps_withhold_the_word_and_say_so() {
        // The republished-word trap (served_liveness.rs): an old `alive`
        // must never read as current.
        let out = registry_json_logic(&[row(json!({
            "harness_session_id": "s-1",
            "liveness": "alive", "liveness_measured_at": z_stamp(121),
        }))]);
        let a = &out["agents"][0];
        assert_eq!(a["liveness"], Value::Null);
        assert_eq!(a["liveness_basis"], "stale");
    }

    #[test]
    fn unmeasured_rows_serve_null_with_never_measured() {
        let out = registry_json_logic(&[row(json!({ "harness_session_id": "s-1" }))]);
        let a = &out["agents"][0];
        assert_eq!(a["liveness"], Value::Null);
        assert_eq!(a["liveness_basis"], "never-measured");
    }

    #[test]
    fn the_projection_keeps_the_eight_stored_keys_and_the_claude_session_rule() {
        let out = registry_json_logic(&[
            row(
                json!({ "harness": "claude", "short_id": "abc12345", "harness_session_id": "uuid-1" }),
            ),
            row(json!({ "harness": "claude", "harness_session_id": "uuid-2" })),
            row(json!({})),
        ]);
        let agents = out["agents"].as_array().unwrap();
        for a in agents {
            for key in [
                "name",
                "session_id",
                "harness_session_id",
                "status",
                "crown_level",
                "crown_scope",
                "spawned_by_session",
                "origin",
                "liveness",
                "liveness_basis",
            ] {
                assert!(a.get(key).is_some(), "projection lost key {key}");
            }
        }
        assert_eq!(
            agents[0]["session_id"], "abc12345",
            "claude transport key wins"
        );
        assert_eq!(
            agents[1]["session_id"], "uuid-2",
            "pane row falls back to the canonical id"
        );
        assert_eq!(agents[2]["session_id"], Value::Null);
        assert_eq!(
            agents[2]["status"], "live",
            "absent status reads live, the dataclass default"
        );
    }
}

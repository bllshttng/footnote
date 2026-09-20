//! `crown-widen`: can an agent add an epic its own session created to its
//! own epic-set crown? One payload kind on the existing `spawn-overlay`
//! verb, the shape `crown-settle` set: graph rows in as JSON, one answer
//! out. The decision lives here, not in Python, so `cli/src/fno` stays
//! call glue.
//!
//! The rule answers exactly one question and refuses everything else. A
//! live agent crowned over a set of epics may add an epic whose
//! `source_session_id` is the agent's own session, and only when it names
//! its full held set plus the new epic. Every other grant still needs an
//! attended shell or a containing crown. A row's `source_session_id` is a
//! birth record the patch door refuses to rewrite, so the answer is
//! verifiable by an external reader - the claim the self-crown refusal
//! guards.

use crate::announce::TERMINAL_STATUSES;
use serde_json::{json, Value};

/// Split a comma-separated crown scope into trimmed, non-blank members,
/// order preserved.
fn split_scope(raw: &str) -> Vec<&str> {
    raw.split(',')
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .collect()
}

/// Can this agent add an epic its own session created to its own crown?
/// See the module doc for the request and answer shapes.
pub fn resolve(payload: &Value) -> Result<Value, String> {
    let requested = payload
        .get("requested")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "crown-widen: payload needs a non-empty requested scope".to_string())?;
    let members = payload
        .get("members")
        .and_then(Value::as_array)
        .ok_or_else(|| "crown-widen: payload needs a members array".to_string())?;
    let caller = payload.get("caller").cloned().unwrap_or(Value::Null);
    let name = caller.get("name").and_then(Value::as_str).unwrap_or("");
    let held = caller
        .get("crown_scope")
        .and_then(Value::as_str)
        .map(split_scope)
        .unwrap_or_default();
    let requested_members = split_scope(requested);
    let sessions: Vec<&str> = ["harness_session_id", "cc_session_id"]
        .iter()
        .filter_map(|f| caller.get(*f).and_then(Value::as_str))
        .filter(|s| !s.trim().is_empty())
        .collect();

    let refused = |hint: Option<&str>| {
        json!({
            "widen": false,
            "added": [],
            "hint": match hint {
                Some(h) => json!(h),
                None => Value::Null,
            },
        })
    };

    // A blank name, an empty held set, no session left, or a terminal
    // caller: today's refusal stands alone, with no hint to add.
    if name.is_empty()
        || held.is_empty()
        || sessions.is_empty()
        || TERMINAL_STATUSES.contains(
            &caller
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )
    {
        return Ok(refused(None));
    }

    // Graph rows by id, one lookup for every rule below.
    let row_for = |id: &str| -> Option<&Value> {
        members
            .iter()
            .filter(|row| row.is_object())
            .find(|row| row.get("id").and_then(Value::as_str) == Some(id))
    };
    let created_by = |id: &str| -> bool {
        row_for(id)
            .and_then(|row| row.get("source_session_id").and_then(Value::as_str))
            .map(|sid| !sid.trim().is_empty() && sessions.contains(&sid))
            .unwrap_or(false)
    };

    let added: Vec<&str> = requested_members
        .iter()
        .copied()
        .filter(|m| !held.contains(m))
        .collect();
    let created: Vec<&str> = added.iter().copied().filter(|m| created_by(m)).collect();
    if created.is_empty() {
        // The caller named no epic it created, so the request is an
        // ordinary grant question today's refusal already answers.
        return Ok(refused(None));
    }

    // Rule: a held epic the request drops would be vacated by the
    // re-scope, and a self-widen never surrenders. Name the full command.
    if let Some(missing) = held.iter().find(|m| !requested_members.contains(m)) {
        let mut all: Vec<&str> = held.clone();
        all.extend(created.iter().copied());
        all.sort_unstable();
        all.dedup();
        let command = all
            .iter()
            .map(|m| format!("--scope {m}"))
            .collect::<Vec<_>>()
            .join(" ");
        return Ok(refused(Some(&format!(
            "to add {} to this session's own crown, name every epic it holds as well: \
             the request drops '{}', so the command is fno agents crown <own handle> {}",
            created.join(", "),
            missing,
            command
        ))));
    }

    // Rule: only a crown over epics widens itself.
    for member in &requested_members {
        let is_epic = row_for(member)
            .map(|row| row.get("type").and_then(Value::as_str) == Some("epic"))
            .unwrap_or(false);
        if !is_epic {
            return Ok(refused(Some(&format!(
                "{member} is not an epic in the graph. Only a crown over epics widens itself."
            ))));
        }
    }

    // Rule: only an epic this session created joins its own crown.
    if let Some(foreign) = added.iter().find(|m| !created.contains(m)) {
        return Ok(refused(Some(&format!(
            "{foreign} was not created by this session. Only an epic this session created \
             joins its own crown. An attended shell or a crown that contains both grants \
             the rest."
        ))));
    }

    Ok(json!({
        "widen": true,
        "added": added,
        "hint": Value::Null,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const S: &str = "aaaaaaaa-1111-4aaa-8aaa-aaaaaaaaaaaa";

    fn payload(requested: &str, held: &str, members: Value) -> Value {
        json!({
            "kind": "crown-widen",
            "requested": requested,
            "caller": {"name": "lead-a", "status": "idle", "crown_scope": held,
                       "harness_session_id": S, "cc_session_id": null},
            "members": members,
        })
    }

    fn row(id: &str, source: &str) -> Value {
        json!({"id": id, "type": "epic", "source_session_id": source})
    }

    #[test]
    fn an_epic_its_own_session_created_joins() {
        // AC1-HP: held e-1, requesting e-1,e-2, e-2 created by this
        // session's id: widen true, added [e-2], hint null.
        let out = resolve(&payload(
            "e-1,e-2",
            "e-1",
            json!([row("e-1", "human"), row("e-2", S)]),
        ))
        .unwrap();
        assert_eq!(out["widen"], true);
        assert_eq!(out["added"], json!(["e-2"]));
        assert!(out["hint"].is_null());
        // The same id in cc_session_id answers the same.
        let mut p = payload(
            "e-1,e-2",
            "e-1",
            json!([row("e-1", "human"), row("e-2", S)]),
        );
        p["caller"]["harness_session_id"] = json!(null);
        p["caller"]["cc_session_id"] = json!(S);
        let out = resolve(&p).unwrap();
        assert_eq!(out["widen"], true);
    }

    #[test]
    fn an_epic_another_session_created_refuses_without_a_hint() {
        // AC2-ERR, first half: the row names another session.
        let out = resolve(&payload(
            "e-1,e-2",
            "e-1",
            json!([
                row("e-1", "human"),
                row("e-2", "bbbbbbbb-2222-4bbb-8bbb-bbbbbbbbbbbb")
            ]),
        ))
        .unwrap();
        assert_eq!(out["widen"], false);
        assert!(out["hint"].is_null());
    }

    #[test]
    fn one_foreign_added_member_refuses_naming_it() {
        // AC2-ERR, second half: e-2 created here, e-3 created elsewhere.
        let out = resolve(&payload(
            "e-1,e-2,e-3",
            "e-1",
            json!([
                row("e-1", "human"),
                row("e-2", S),
                row("e-3", "bbbbbbbb-2222-4bbb-8bbb-bbbbbbbbbbbb")
            ]),
        ))
        .unwrap();
        assert_eq!(out["widen"], false);
        let hint = out["hint"].as_str().unwrap();
        assert!(hint.contains("e-3"), "{hint}");
        assert!(hint.contains("was not created by this session"), "{hint}");
    }

    #[test]
    fn a_caller_with_no_crown_refuses() {
        // AC3-EDGE: no crown.
        let out = resolve(&payload("e-2", "", json!([row("e-2", S)]))).unwrap();
        assert_eq!(out["widen"], false);
    }

    #[test]
    fn a_terminal_caller_refuses() {
        // AC3-EDGE: exited.
        let mut p = payload(
            "e-1,e-2",
            "e-1",
            json!([row("e-1", "human"), row("e-2", S)]),
        );
        p["caller"]["status"] = json!("exited");
        let out = resolve(&p).unwrap();
        assert_eq!(out["widen"], false);
    }

    #[test]
    fn a_request_that_drops_a_held_epic_names_the_full_command() {
        // AC3-EDGE: e-1 held, request names only e-2.
        let out = resolve(&payload("e-2", "e-1", json!([row("e-2", S)]))).unwrap();
        assert_eq!(out["widen"], false);
        let hint = out["hint"].as_str().unwrap();
        assert!(hint.contains("name every epic it holds"), "{hint}");
        assert!(hint.contains("--scope e-1"), "{hint}");
        assert!(hint.contains("--scope e-2"), "{hint}");
    }

    #[test]
    fn a_non_epic_member_refuses_naming_it() {
        // AC3-EDGE: a feature row, and a missing row.
        let out = resolve(&payload(
            "e-1,f-9",
            "e-1",
            json!([row("e-1", "human"),
                   {"id": "f-9", "type": "feature", "source_session_id": S}]),
        ))
        .unwrap();
        assert_eq!(out["widen"], false);
        assert!(out["hint"].as_str().unwrap().contains("f-9 is not an epic"),);
        let out = resolve(&payload(
            "e-1,g-7",
            "e-1",
            json!([row("e-1", "human"), Value::Null]),
        ))
        .unwrap();
        assert_eq!(out["widen"], false);
        assert!(out["hint"].as_str().unwrap().contains("g-7 is not an epic"));
    }

    #[test]
    fn blank_session_ids_never_match() {
        // AC3-EDGE: every session field blank on both sides.
        let mut p = payload(
            "e-1,e-2",
            "e-1",
            json!([row("e-1", "human"), row("e-2", "")]),
        );
        p["caller"]["harness_session_id"] = json!("");
        let out = resolve(&p).unwrap();
        assert_eq!(out["widen"], false);
    }

    #[test]
    fn a_blank_requested_scope_is_an_error() {
        // AC3-EDGE: no `requested` makes the verb exit 2.
        let error = resolve(&json!({
            "kind": "crown-widen",
            "caller": {"name": "lead-a", "status": "idle", "crown_scope": "e-1"},
            "members": [],
        }))
        .unwrap_err();
        assert!(error.contains("requested"), "{error}");
        assert!(resolve(&payload("", "e-1", json!([]))).is_err());
    }

    #[test]
    fn members_that_are_not_an_array_are_an_error() {
        let error = resolve(&json!({
            "kind": "crown-widen", "requested": "e-2",
            "caller": {"name": "lead-a", "crown_scope": "e-1"},
            "members": "nope",
        }))
        .unwrap_err();
        assert!(error.contains("members array"), "{error}");
    }
}

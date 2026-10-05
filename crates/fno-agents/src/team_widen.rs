//! `team-widen`: may this session edit its own epic-set team? One
//! payload kind on the existing `spawn-overlay` verb, the shape
//! `team-settle` set: graph rows in as JSON, one answer out. The
//! decision lives here, not in Python, so `cli/src/fno` stays call glue.
//!
//! The rule answers exactly one question and refuses everything else. A
//! live agent teamed over a set of epics may add an epic whose
//! `source_session_id` is the agent's own session, and may drop a held
//! epic whose graph row reads terminal (done or superseded). Every other
//! grant still needs an attended shell or a containing team. A row's
//! `source_session_id` is a birth record the patch door refuses to
//! rewrite, so the answer is verifiable by an external reader - the claim
//! the self-team refusal guards.

use serde_json::{json, Value};

/// Split a comma-separated team scope into trimmed, non-blank members,
/// order preserved.
fn split_scope(raw: &str) -> Vec<&str> {
    raw.split(',')
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .collect()
}

/// May this session edit its own epic-set team (an add it created, a
/// drop of a closed epic)? See the module doc for the request and answer
/// shapes.
pub fn resolve(payload: &Value) -> Result<Value, String> {
    let requested = payload
        .get("requested")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "team-widen: payload needs a non-empty requested scope".to_string())?;
    let members = payload
        .get("members")
        .and_then(Value::as_array)
        .ok_or_else(|| "team-widen: payload needs a members array".to_string())?;
    let caller = payload.get("caller").cloned().unwrap_or(Value::Null);
    let name = caller.get("name").and_then(Value::as_str).unwrap_or("");
    let held = caller
        .get("crown_scope")
        .and_then(Value::as_str)
        .map(split_scope)
        .unwrap_or_default();
    let requested_members = split_scope(requested);
    let target = payload
        .get("target")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let grantor_held = caller
        .get("crown_grantor")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    let sessions: Vec<&str> = ["harness_session_id", "cc_session_id"]
        .iter()
        .filter_map(|f| caller.get(*f).and_then(Value::as_str))
        .filter(|s| !s.trim().is_empty())
        .collect();

    let refused = |hint: Option<&str>| {
        json!({
            "widen": false,
            "added": [],
            "dropped": [],
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
        || crate::row_verdict::finished_json(&caller)
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
    let terminal = |id: &str| -> bool {
        row_for(id)
            .map(crate::graph_store::is_terminal_entry)
            .unwrap_or(false)
    };

    // Rule: only a team over epics widens itself. This runs before the
    // created-set check below, because a member with NO row is not an epic
    // either: leave it later and an unknown id falls into the hintless
    // ordinary-grant refusal, naming nothing the caller can act on.
    for member in &requested_members {
        let is_epic = row_for(member)
            .map(|row| row.get("type").and_then(Value::as_str) == Some("epic"))
            .unwrap_or(false);
        if !is_epic {
            return Ok(refused(Some(&format!(
                "{member} is not an epic in the graph. Only a team over epics widens itself."
            ))));
        }
    }

    let added: Vec<&str> = requested_members
        .iter()
        .copied()
        .filter(|m| !held.contains(m))
        .collect();
    let dropped: Vec<&str> = held
        .iter()
        .copied()
        .filter(|m| !requested_members.contains(m))
        .collect();
    let created: Vec<&str> = added.iter().copied().filter(|m| created_by(m)).collect();

    // Rule: this answer belongs to the caller's own team. When the
    // resolved target names another row, the request is an ordinary grant
    // question the caller's containment check already answered.
    if target != name {
        return Ok(refused(None));
    }

    // Self re-scope from here on. A team is stamped by a grantor, never
    // self-declared: an edit that neither adds nor drops anything is the
    // succession shape, and succession runs through `spawn --crown`.
    if added.is_empty() && dropped.is_empty() {
        return Ok(refused(Some(&format!(
            "refusing to team {name:?}: that is this session, and a team is stamped by a \
             grantor, never self-declared. The row would record itself as its own grantor, \
             which is exactly the claim an external reader cannot verify. Ask a lead whose \
             scope contains {requested:?}, or team a different row."
        ))));
    }

    if !added.is_empty() && created.is_empty() {
        // The caller named no epic it created, so the request is an
        // ordinary grant question today's refusal already answers.
        return Ok(refused(None));
    }

    // Rule: a held epic the request drops must already be closed. A live
    // member (or one with no row to prove it closed) is territory the lead
    // still rules, so the refusal names it and prints the command that
    // would succeed: every held member not provably closed, plus the adds.
    if let Some(missing) = dropped.iter().find(|m| !terminal(m)) {
        let mut keep: Vec<&str> = held.iter().copied().filter(|m| !terminal(m)).collect();
        keep.extend(created.iter().copied());
        keep.sort_unstable();
        keep.dedup();
        let command = keep
            .iter()
            .map(|m| format!("--scope {m}"))
            .collect::<Vec<_>>()
            .join(" ");
        let lead = if created.is_empty() {
            format!(
                "the request drops '{}', which is not done: a team edit drops an epic \
                 only once it reads done or superseded",
                missing
            )
        } else {
            format!(
                "to add {} to this session's own team, name every epic it holds as well: \
                 the request drops '{}'",
                created.join(", "),
                missing
            )
        };
        return Ok(refused(Some(&format!(
            "{lead}, so the command is fno agents org promote <own handle> {command}"
        ))));
    }

    // Rule: only an epic this session created joins its own team.
    if let Some(foreign) = added.iter().find(|m| !created.contains(m)) {
        return Ok(refused(Some(&format!(
            "{foreign} was not created by this session. Only an epic this session created \
             joins its own team. An attended shell or a team that contains both grants \
             the rest."
        ))));
    }

    // The grantor survives the self edit: echo the recorded one so the
    // Python stamp keeps the upstream grant instead of self-declaring.
    let grantor = if grantor_held.is_empty() {
        name
    } else {
        grantor_held
    };
    Ok(json!({
        "widen": true,
        "added": added,
        "dropped": dropped,
        "grantor": grantor,
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
            "target": "lead-a",
            "caller": {"name": "lead-a", "status": "idle", "crown_scope": held,
                       "harness_session_id": S, "cc_session_id": null},
            "members": members,
        })
    }

    fn row(id: &str, source: &str) -> Value {
        json!({"id": id, "type": "epic", "source_session_id": source})
    }

    #[test]
    fn join_rows() {
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
    fn caller_rows() {
        // AC3-EDGE: no team.
        let out = resolve(&payload("e-2", "", json!([row("e-2", S)]))).unwrap();
        assert_eq!(out["widen"], false);

        // AC3-EDGE: exited.
        let mut p = payload(
            "e-1,e-2",
            "e-1",
            json!([row("e-1", "human"), row("e-2", S)]),
        );
        p["caller"]["status"] = json!("exited");
        let out = resolve(&p).unwrap();
        assert_eq!(out["widen"], false);

        // The reversible word never refuses a live caller: an Orphaned
        // row with a live pid still widens its own team.
        let mut p = payload(
            "e-1,e-2",
            "e-1",
            json!([row("e-1", "human"), row("e-2", S)]),
        );
        p["caller"]["status"] = json!("orphaned");
        p["caller"]["pid"] = json!(std::process::id());
        let out = resolve(&p).unwrap();
        assert_eq!(out["widen"], true);
        assert_eq!(out["added"], json!(["e-2"]));

        // AC3-EDGE: no `requested` makes the verb exit 2.
        let error = resolve(&json!({
            "kind": "crown-widen",
            "caller": {"name": "lead-a", "status": "idle", "crown_scope": "e-1"},
            "members": [],
        }))
        .unwrap_err();
        assert!(error.contains("requested"), "{error}");
        assert!(resolve(&payload("", "e-1", json!([]))).is_err());

        let error = resolve(&json!({
            "kind": "crown-widen", "requested": "e-2",
            "caller": {"name": "lead-a", "crown_scope": "e-1"},
            "members": "nope",
        }))
        .unwrap_err();
        assert!(error.contains("members array"), "{error}");
    }

    #[test]
    fn drop_rows() {
        // AC3-EDGE: e-1 held, request names only e-2.
        let out = resolve(&payload("e-2", "e-1", json!([row("e-2", S)]))).unwrap();
        assert_eq!(out["widen"], false);
        let hint = out["hint"].as_str().unwrap();
        assert!(hint.contains("name every epic it holds"), "{hint}");
        assert!(hint.contains("--scope e-1"), "{hint}");
        assert!(hint.contains("--scope e-2"), "{hint}");

        // AC1-HP (drop): held e-1,e-2 with e-2 done, requesting e-1:
        // widen true, added [], dropped [e-2], grantor echoes the
        // recorded one instead of self-declaring.
        let out = resolve(&grantor_payload(
            "e-1",
            "e-1,e-2",
            json!([
                {"id": "e-1", "type": "epic", "status": "ready"},
                {"id": "e-2", "type": "epic", "status": "done"},
            ]),
            "human",
        ))
        .unwrap();
        assert_eq!(out["widen"], true);
        assert_eq!(out["added"], json!([]));
        assert_eq!(out["dropped"], json!(["e-2"]));
        assert_eq!(out["grantor"], "human");

        // AC1-HP (drop + add): e-2 superseded drops, e-3 self-created adds.
        let out = resolve(&payload(
            "e-1,e-3",
            "e-1,e-2",
            json!([
                {"id": "e-1", "type": "epic", "status": "ready"},
                {"id": "e-2", "type": "epic", "status": "superseded"},
                row("e-3", S),
            ]),
        ))
        .unwrap();
        assert_eq!(out["widen"], true);
        assert_eq!(out["added"], json!(["e-3"]));
        assert_eq!(out["dropped"], json!(["e-2"]));

        // AC2-ERR (drop): e-2 still live; the hint names it and prints
        // the full command (held minus closed members plus the adds).
        let out = resolve(&payload(
            "e-1",
            "e-1,e-2",
            json!([
                {"id": "e-1", "type": "epic", "status": "ready"},
                {"id": "e-2", "type": "epic", "status": "ready"},
            ]),
        ))
        .unwrap();
        assert_eq!(out["widen"], false);
        let hint = out["hint"].as_str().unwrap();
        assert!(hint.contains("e-2"), "{hint}");
        assert!(hint.contains("done or superseded"), "{hint}");
        assert!(hint.contains("--scope e-1 --scope e-2"), "{hint}");

        // AC2-ERR (drop): no graph row, no terminal proof; same refusal.
        let out = resolve(&payload(
            "e-1",
            "e-1,e-2",
            json!([{"id": "e-1", "type": "epic", "status": "ready"}]),
        ))
        .unwrap();
        assert_eq!(out["widen"], false);
        let hint = out["hint"].as_str().unwrap();
        assert!(hint.contains("e-2"), "{hint}");

        // AC2-ERR: requested == held adds and drops nothing; the moved
        // team.py text names the self-grantor claim it prevents.
        let out = resolve(&payload(
            "e-1",
            "e-1",
            json!([{"id": "e-1", "type": "epic", "status": "ready"}]),
        ))
        .unwrap();
        assert_eq!(out["widen"], false);
        let hint = out["hint"].as_str().unwrap();
        assert!(hint.contains("never self-declared"), "{hint}");

        // AC6-EDGE: the widen answer belongs to this session's own row.
        // Even a droppable done epic is refused hintless when the resolved
        // target is another row - the ordinary grant keeps its answer.
        let mut p = grantor_payload(
            "e-1",
            "e-1,e-2",
            json!([
                {"id": "e-1", "type": "epic", "status": "ready"},
                {"id": "e-2", "type": "epic", "status": "done"},
            ]),
            "human",
        );
        p["target"] = json!("lead-c");
        let out = resolve(&p).unwrap();
        assert_eq!(out["widen"], false);
        assert!(out["hint"].is_null());

        // AC3-EDGE: crown_grantor unset on the row; the echo is the name.
        let out = resolve(&payload(
            "e-1",
            "e-1,e-2",
            json!([
                {"id": "e-1", "type": "epic", "status": "ready"},
                {"id": "e-2", "type": "epic", "status": "done"},
            ]),
        ))
        .unwrap();
        assert_eq!(out["widen"], true);
        assert_eq!(out["grantor"], "lead-a");
    }

    fn grantor_payload(requested: &str, held: &str, members: Value, grantor: &str) -> Value {
        let mut p = payload(requested, held, members);
        p["caller"]["crown_grantor"] = json!(grantor);
        p
    }
}

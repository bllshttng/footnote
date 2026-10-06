//! Thread promotion and the registry effects shared by spawn carriers.

use serde_json::{json, Map, Value};

pub fn resolve(payload: &Value) -> Result<Value, String> {
    match payload.get("op").and_then(Value::as_str) {
        Some("seed") => {
            let message = payload["message"].as_str().unwrap_or("");
            let scope = payload["scope"].as_str().filter(|s| !s.is_empty());
            let typed = payload["level"].is_u64()
                && scope.is_some()
                && payload["revive"].as_bool() != Some(true);
            let message = if typed {
                let lead = crate::provider::render_verb_seed(
                    &format!("/fno:lead {}", scope.unwrap()),
                    payload["harness"].as_str().unwrap_or("claude"),
                );
                format!("{lead}\n{message}")
            } else {
                message.to_owned()
            };
            Ok(json!({"message": message, "typed": typed}))
        }
        Some("apply") => apply(payload),
        Some("journal") => Ok(journal(payload)),
        Some("receipt") => Ok(receipt(payload)),
        Some("validate") => Ok(json!({"refusal": validate(payload)})),
        _ => Err("spawn-team: expected seed|apply|journal|receipt|validate".into()),
    }
}

fn validate(payload: &Value) -> Option<String> {
    let level = &payload["level"];
    let scope = &payload["scope"];
    let level_repr = payload["level_repr"].as_str().unwrap_or("None");
    let scope_repr = payload["scope_repr"].as_str().unwrap_or("None");
    if level.is_null() && scope.is_null() {
        return None;
    }
    if level.is_null() || scope.is_null() {
        return Some(format!(
            "a crown needs both level and scope; got level={level_repr} scope={scope_repr}"
        ));
    }
    if payload["level_is_int"] != true {
        return Some(format!("crown level must be an int 0..2; got {level_repr}"));
    }
    let Some(level) = level
        .as_str()
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|l| *l <= 2)
    else {
        return Some(format!("crown level must be 0..2 (0 several projects, 1 one project, 2 one epic); got {level_repr}"));
    };
    let Some(scope) = scope
        .as_str()
        .filter(|s| payload["scope_is_str"] == true && !s.trim().is_empty())
    else {
        return Some(format!(
            "crown scope must be a nonblank id; got {scope_repr}"
        ));
    };
    let mut members: Vec<&str> = scope
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    members.sort_unstable();
    members.dedup();
    let canonical = members.join(",");
    if canonical != scope {
        return Some(format!("crown scope must be canonical (sorted, deduped, no blank members); got {scope_repr}, want {}", crate::claude_ask::py_repr(&canonical)));
    }
    if members.len() > 1 && level != 0 && level != 2 {
        return Some(format!("a scope naming {} members is level 0 (a portfolio of projects) or 2 (a set of epics), not {level}", members.len()));
    }
    let projects: Vec<&str> = payload["projects"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if level == 0 && members.len() > 1 && projects.is_empty() {
        return Some(format!("level 0 is a portfolio of PROJECTS, but no member of {scope_repr} resolves to a configured project"));
    }
    if level == 2 && !projects.is_empty() {
        return Some(format!(
            "level 2 is a SET OF EPICS, but {} resolve(s) to a configured project",
            projects.join(", ")
        ));
    }
    None
}

fn apply(payload: &Value) -> Result<Value, String> {
    let rows = payload["rows"]
        .as_array()
        .ok_or("spawn-team: rows must be an array")?;
    let heir_index = if payload["stamp"].as_bool() == Some(true) {
        let identity = crate::team_identity::resolve(&json!({
            "rows": rows,
            "expect": {"name": payload["heir"],
                "harness_session_id": payload["heir_identity"]["session_id"]},
        }))?;
        if identity["matched"] != true {
            return Ok(json!({"outcome": "declined", "updates": {}, "vacated": [],
                "heir_index": null}));
        }
        identity["index"].as_u64().map(|i| i as usize)
    } else {
        None
    };
    let mut settlement = payload.clone();
    if heir_index.is_some() {
        // A carrier may stamp at mint, before the seed turn runs. Its own
        // verified heir is not a competing holder during settlement.
        settlement["exclude_name"] = payload["heir"].clone();
    }
    let answer = crate::team_settle::resolve(&settlement)?;
    let outcome = answer["outcome"]
        .as_str()
        .ok_or("spawn-team: missing outcome")?;
    let mut updates = Map::new();
    let mut vacated = Vec::new();
    for (field, cause) in [
        ("clear_terminal_rows", "holder_terminal"),
        ("vacate_rows", "succession"),
    ] {
        for index in answer[field]
            .as_array()
            .ok_or("spawn-team: missing row indexes")?
        {
            let i = index.as_u64().ok_or("spawn-team: invalid row index")? as usize;
            rows.get(i).ok_or("spawn-team: row index out of bounds")?;
            updates.insert(
                i.to_string(),
                json!({"crown_level": null,
                "crown_scope": null, "crown_grantor": null}),
            );
            vacated.push(json!([i, cause, {}]));
        }
    }
    reown(rows, &answer, &mut updates, &mut vacated)?;
    if let Some(i) = heir_index {
        let stamp = if outcome == "declined" {
            json!({"crown_level": null, "crown_scope": null, "crown_grantor": null})
        } else {
            json!({"crown_level": payload["level"], "crown_scope": payload["scope"],
                "crown_grantor": payload["grantor"]})
        };
        updates
            .entry(i.to_string())
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .unwrap()
            .extend(stamp.as_object().unwrap().clone());
    }
    Ok(
        json!({"outcome": outcome, "updates": updates, "vacated": vacated,
        "heir_index": heir_index}),
    )
}

fn reown(
    rows: &[Value],
    answer: &Value,
    updates: &mut Map<String, Value>,
    vacated: &mut Vec<Value>,
) -> Result<(), String> {
    if answer["reown_owner"].is_null() {
        return Ok(());
    }
    for index in answer["reown_rows"]
        .as_array()
        .ok_or("spawn-team: missing reown indexes")?
    {
        let i = index.as_u64().ok_or("spawn-team: invalid reown index")? as usize;
        let row = rows.get(i).ok_or("spawn-team: reown index out of bounds")?;
        // Reown moves an existing owner only. A block forked onto a
        // provenance-less row has no origin, and the typed registry reader
        // rejects it.
        let Some(provenance) = row["spawn_provenance"].as_object() else {
            continue;
        };
        let mut provenance = provenance.clone();
        provenance.insert("owner".into(), answer["reown_owner"].clone());
        updates.entry(i.to_string()).or_insert_with(|| json!({}))["spawn_provenance"] =
            json!(provenance);
        vacated.push(json!([i, "reowned", {"spawn_provenance": provenance}]));
    }
    Ok(())
}

fn journal(payload: &Value) -> Value {
    let mut events = Vec::new();
    for vacated in payload["vacated"].as_array().into_iter().flatten() {
        let row = &vacated[0];
        let cause = vacated[1].as_str().unwrap_or("");
        let (kind, data) = if cause == "reowned" {
            (
                "agent_court_reowned",
                json!({"scope": payload["scope"],
                "successor": payload["name"], "child": row["name"]}),
            )
        } else {
            (
                "agent_crown_vacated",
                json!({"scope": payload["scope"],
                "level": row["crown_level"], "holder": row["name"],
                "holder_session": row["harness_session_id"], "grantor": row["crown_grantor"],
                "cause": cause, "successor": if cause == "succession" {payload["name"].clone()} else {Value::Null}}),
            )
        };
        events.push(json!({"kind": kind, "data": data}));
    }
    let armed = matches!(payload["outcome"].as_str(), Some("granted" | "succeeded"));
    if armed {
        events.push(json!({"kind": "agent_crowned", "data": {
            "name": payload["name"], "level": payload["level"],
            "scope": payload["scope"], "grantor": payload["grantor"],
            "vacated_scope": null, "vacated_level": null, "stranded_subordinates": []}}));
    }
    json!({"events": events, "arm_missions": armed})
}

fn receipt(payload: &Value) -> Value {
    let scope = payload["scope"].as_str().unwrap_or("");
    let name = payload["name"].as_str().unwrap_or("");
    let mut notices = Vec::new();
    if payload["found"] != true {
        notices.push(format!("spawn: promotion over {scope:?} NOT applied: no matching registry identity for {name:?}; grant it with `fno agents org promote`"));
    } else if payload["outcome"] == "declined" {
        notices.push(format!("spawn: promotion declined (scope {scope:?} already held by a live row); the worker launched without a role."));
    } else {
        if payload["outcome"] == "succeeded" {
            let mut vacated: Vec<&str> = payload["vacated"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|v| v[1] == "succession")
                .filter_map(|v| v[0]["name"].as_str())
                .collect();
            vacated.sort_unstable();
            vacated.dedup();
            let tail = if vacated.contains(&payload["caller"].as_str().unwrap_or("")) {
                " You no longer hold it."
            } else {
                ""
            };
            notices.push(format!(
                "spawn: promotion over {scope:?} transferred from {} to {name} (succession).{tail}",
                vacated.join(", ")
            ));
        }
        if payload["armed"] == false {
            let error = payload["arm_error"].as_str().unwrap_or("");
            let reason = if error.is_empty() {
                "; lead loop disabled".to_owned()
            } else {
                format!(": {error}")
            };
            notices.push(format!("spawn: promotion over {scope:?} recorded, but the lead loop manifest was NOT armed{reason}"));
        }
        let typed = if payload["typed"] == true {
            "lead typed"
        } else {
            "lead NOT typed"
        };
        notices.push(format!("spawn: promotion over {scope:?} recorded; {typed}"));
    }
    json!({"notices": notices})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_decisions_match_captured_answers() {
        let fixtures: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/team_spawn.json"
        )))
        .unwrap();
        for case in fixtures["cases"].as_array().unwrap() {
            assert_eq!(
                resolve(&case["payload"]).unwrap(),
                case["answer"],
                "{}",
                case["payload"]
            );
        }
    }

    #[test]
    fn seed_preserves_payload_and_the_receiving_harness_verb() {
        for (harness, verb) in [
            ("claude", "/fno:lead"),
            ("codex", "$fno:lead"),
            ("opencode", "/fno:lead"),
            ("pi", "/skill:lead"),
        ] {
            let ask = json!({"op": "seed", "message": "ship\nkeep bytes", "level": 2,
                "scope": "epic-a", "harness": harness, "revive": false});
            let answer = resolve(&ask).unwrap();
            assert_eq!(
                answer["message"],
                format!("{verb} epic-a\nship\nkeep bytes")
            );
            assert_eq!(answer["typed"], true);
            let mut revived = ask.clone();
            revived["revive"] = json!(true);
            assert_eq!(resolve(&revived).unwrap()["message"], "ship\nkeep bytes");
        }
    }

    #[test]
    fn rebound_heir_cannot_clear_the_predecessor() {
        let answer = resolve(&json!({"op": "apply", "stamp": true, "scope": "epic-a",
            "heir": "heir", "heir_identity": {"session_id": "original"},
            "rows": [{"name": "heir", "harness_session_id": "rebound"},
                {"name": "predecessor", "crown_level": 2, "crown_scope": "epic-a"}]}))
        .unwrap();
        assert_eq!(answer["outcome"], "declined");
        assert_eq!(answer["updates"], json!({}));
        assert_eq!(answer["vacated"], json!([]));
    }

    #[test]
    fn reown_never_forks_provenance_onto_a_bare_row() {
        let (mut updates, mut vacated) = (Map::new(), Vec::new());
        let answer = json!({"reown_rows": [0], "reown_owner": {"kind": "session"}});
        reown(
            &[json!({"name": "w5"})],
            &answer,
            &mut updates,
            &mut vacated,
        )
        .unwrap();
        assert!(updates.is_empty() && vacated.is_empty());
    }
}

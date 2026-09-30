//! `court-rivals`: the court view's rivalry scan, answered from the SAME
//! ladder-aware rule the grant path enforces (`loop_king::crown_rivals`), so
//! the view and the grant-time refusal cannot disagree. Reached as payload
//! kind `court-rivals` on the spawn-overlay verb.
//!
//! Input: `rows` of (name, crown_scope, crown_level); rows without a
//! non-blank `crown_scope` claim nothing. Output: one rival PAIR per
//! double-ruled territory, `{"members": [...], "holders": [a, b]}` - one
//! entry PER PAIR, never a merged group: rivalry is not transitive (A/e-1,
//! B/e-1,e-2, C/e-2 rivals A-B and B-C only), so a group would claim three
//! rows hold what no pair does.
//!
//! The project map degrades the way `loop_king`'s walk documents: an
//! unreadable map downgrades every check to raw member overlap, the same
//! rule as the same-rung case, never a silent all-clear.

use serde_json::{json, Value};
use std::collections::HashMap;

pub fn resolve(payload: &Value) -> Result<Value, String> {
    let empty = HashMap::new();
    // The caller's own project table outranks the cwd read: tests plant a
    // config the overlay process cannot see, and a court run from a foreign
    // directory still answers from ITS repo's table.
    let injected: HashMap<String, String> = payload
        .get("projects")
        .and_then(Value::as_object)
        .map(|table| {
            table
                .iter()
                .filter_map(|(alias, canon)| {
                    canon
                        .as_str()
                        .map(|canon| (alias.clone(), canon.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    let projects = if injected.is_empty() && payload.get("projects").is_none() {
        crate::king_board::project_map(&std::env::current_dir().unwrap_or_default())
            .unwrap_or_else(|_| empty.clone())
    } else {
        injected
    };
    resolve_with_projects(payload, &projects)
}

/// [`resolve`] with the project map injected, so unit tests answer from a
/// known territory table instead of the caller's cwd.
pub fn resolve_with_projects(
    payload: &Value,
    projects: &HashMap<String, String>,
) -> Result<Value, String> {
    let rows = payload
        .get("rows")
        .and_then(Value::as_array)
        .ok_or_else(|| "court-rivals: payload needs a rows array".to_string())?;
    let mut claims: Vec<(String, Option<u32>, String)> = Vec::new();
    for row in rows {
        let scope = row
            .get("crown_scope")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if scope.is_empty() {
            continue;
        }
        claims.push((
            row.get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            row.get("crown_level")
                .and_then(Value::as_u64)
                .map(|n| n as u32),
            scope.to_string(),
        ));
    }
    let mut pairs = Vec::new();
    for i in 0..claims.len() {
        for j in (i + 1)..claims.len() {
            let (name_i, level_i, scope_i) = &claims[i];
            let (name_j, level_j, scope_j) = &claims[j];
            if !crate::loop_king::crown_rivals_pub(scope_i, *level_i, scope_j, *level_j, projects) {
                continue;
            }
            let members_i = crate::loop_king::territory_members(scope_i, projects);
            let shared: Vec<String> = crate::loop_king::territory_members(scope_j, projects)
                .into_iter()
                .filter(|member| members_i.contains(member))
                .collect();
            pairs.push(json!({"scope": shared.join(","), "holders": [name_i, name_j]}));
        }
    }
    Ok(json!({"pairs": pairs}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn scan(rows: Value, projects: &HashMap<String, String>) -> Value {
        resolve_with_projects(&json!({"kind": "court-rivals", "rows": rows}), projects).unwrap()
    }

    #[test]
    fn the_rivalry_table_answers_per_pair() {
        // One scan pins the rule's table: a portfolio over two project kings
        // rivals EACH (one entry per pair, never a group); disjoint
        // territories rival nothing; a portfolio and a non-member court stay
        // legitimate; blank scopes claim nothing; a missing rows array is an
        // error, never an empty answer.
        let projects = HashMap::from([
            ("alpha".to_string(), "alpha".to_string()),
            ("beta".to_string(), "beta".to_string()),
        ]);
        let out = scan(
            json!([
                {"name": "portfolio", "crown_scope": "alpha,beta", "crown_level": 2},
                {"name": "king-a", "crown_scope": "alpha", "crown_level": 0},
                {"name": "king-b", "crown_scope": "beta", "crown_level": 0}
            ]),
            &projects,
        );
        let pairs = out["pairs"].as_array().unwrap();
        assert_eq!(pairs.len(), 2, "two rivals of the portfolio, never a group");
        let holders: Vec<Vec<&str>> = pairs
            .iter()
            .map(|p| {
                p["holders"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|h| h.as_str().unwrap())
                    .collect()
            })
            .collect();
        assert!(holders.contains(&vec!["portfolio", "king-a"]));
        assert!(holders.contains(&vec!["portfolio", "king-b"]));
        assert_eq!(
            pairs[0]["scope"].as_str().unwrap(),
            "alpha",
            "the pair names the territory it actually shares"
        );
        let out = scan(
            json!([
                {"name": "king-a", "crown_scope": "alpha", "crown_level": 0},
                {"name": "king-b", "crown_scope": "beta", "crown_level": 0}
            ]),
            &projects,
        );
        assert_eq!(out["pairs"], json!([]));
        let out = scan(
            json!([
                {"name": "portfolio", "crown_scope": "alpha,beta", "crown_level": 2},
                {"name": "court-king", "crown_scope": "e-court", "crown_level": 2}
            ]),
            &projects,
        );
        // e-court resolves to no configured project; its members rival
        // nothing the portfolio holds.
        assert_eq!(out["pairs"], json!([]));
        let out = scan(
            json!([
                {"name": "blank", "crown_scope": "  ", "crown_level": 2},
                {"name": "none"}
            ]),
            &HashMap::new(),
        );
        assert_eq!(out["pairs"], json!([]));
        assert!(resolve_with_projects(&json!({"kind": "court-rivals"}), &HashMap::new()).is_err());
    }
}

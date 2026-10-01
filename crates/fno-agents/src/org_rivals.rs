//! `org-rivals`: the org view's rivalry scan, answered from the SAME
//! ladder-aware rule the grant path enforces (`loop_lead::team_rivals`), so
//! the view and the grant-time refusal cannot disagree. Reached as payload
//! kind `org-rivals` on the spawn-overlay verb.
//!
//! Input: `rows` of (name, crown_scope, crown_level); rows without a
//! non-blank `crown_scope` claim nothing. Output: one rival PAIR per
//! double-ruled territory, `{"members": [...], "holders": [a, b]}` - one
//! entry PER PAIR, never a merged group: rivalry is not transitive (A/e-1,
//! B/e-1,e-2, C/e-2 rivals A-B and B-C only), so a group would claim three
//! rows hold what no pair does.
//!
//! The project map degrades the way `loop_lead`'s walk documents: an
//! unreadable map downgrades every check to raw member overlap, the same
//! rule as the same-rung case, never a silent all-clear.

use serde_json::{json, Value};
use std::collections::HashMap;

pub fn resolve(payload: &Value) -> Result<Value, String> {
    // The caller's own project table outranks the cwd read: tests plant a
    // config the overlay process cannot see, and a org run from a foreign
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
        crate::org_board::project_map(&std::env::current_dir().unwrap_or_default())
            .unwrap_or_default()
    } else {
        injected
    };
    scan(&payload, &projects)
}

fn scan(payload: &Value, projects: &HashMap<String, String>) -> Result<Value, String> {
    let rows = payload
        .get("rows")
        .and_then(Value::as_array)
        .ok_or_else(|| "org-rivals: payload needs a rows array".to_string())?;
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
            if !crate::loop_lead::team_rivals_pub(scope_i, *level_i, scope_j, *level_j, projects) {
                continue;
            }
            let members_i = crate::loop_lead::territory_members(scope_i, projects);
            let shared: Vec<String> = crate::loop_lead::territory_members(scope_j, projects)
                .into_iter()
                .filter(|member| members_i.contains(member))
                .collect();
            pairs.push(json!({"scope": shared.join(","), "holders": [name_i, name_j]}));
        }
    }
    Ok(json!({"pairs": pairs}))
}

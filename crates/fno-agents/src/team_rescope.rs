//! `team-rescope`: the crown re-scope's team-name effect, reached as payload
//! kind `team-rescope` on the existing `spawn-overlay` verb (law d-fe66560a
//! bars new top-level verbs, hidden ones included).
//!
//! `org promote` re-scoping a crowned row used to leave the team's name
//! record keyed on the vacated scope: the lead landed anonymous, and the name
//! came back only through a manual `org checkin --keep-name-from` (candor
//! dropped its name twice on 2026-10-04). The promote calls this kind after
//! its registry commit: a recorded name moves to the landing scope, and an
//! unnamed landing team takes the holder row's own name when it is
//! people-shaped. Advisory: the registry commit is the authority, so a store
//! failure is the answer's `reason`, never a failed promote (the same
//! contract team-settle's name effect keeps). Coverage rides the
//! `team_names` tests, the shrink-only suite's net-zero rule.

use serde_json::{json, Value};
use std::path::Path;

/// The one answer shape: `carried` says a recorded name moved, `named` is the
/// display string a fresh carry stamped, `reason` names why neither happened.
pub fn resolve_at(payload: &Value, store: &Path, registry: &Path) -> Result<Value, String> {
    let old = payload
        .get("old_scope")
        .and_then(Value::as_str)
        .unwrap_or("");
    let new = payload
        .get("new_scope")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "team-rescope: payload needs a non-empty new_scope".to_string())?;
    let candidate = payload
        .get("candidate")
        .and_then(Value::as_str)
        .unwrap_or("");
    let outcome = (|| -> Result<(bool, Option<String>), String> {
        let carried = if old.trim().is_empty() {
            false
        } else {
            crate::team_names::carry_rescope(store, registry, old, new)?
        };
        let named = crate::team_names::carry_holder_name(store, registry, new, candidate)?;
        Ok((carried, named))
    })();
    Ok(match outcome {
        Ok((carried, named)) => json!({
            "carried": carried,
            "named": named,
            "reason": Value::Null,
        }),
        Err(reason) => json!({"carried": false, "named": Value::Null, "reason": reason}),
    })
}

/// The ambient-home entry. No declared home (a test) answers carried=false
/// with the reason, like team-settle's tolerance.
pub fn resolve(payload: &Value) -> Result<Value, String> {
    match crate::paths::AgentsHome::from_env_opt() {
        Some(home) => resolve_at(payload, &home.team_names_json(), &home.registry_json()),
        None => Ok(json!({
            "carried": false,
            "named": Value::Null,
            "reason": "no agents home declared",
        })),
    }
}

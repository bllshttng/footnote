//! The keeper's `plan_docs` method: the bridge between the store method table
//! and the plan-doc writer, so the Python callers are clients and no second
//! writer leg exists.

use std::path::Path;

use serde_json::{json, Map, Value};

use crate::graph_keeper::{cached_entries, StoreState};
use crate::graph_store::StoreError;

fn opt_str<'a>(p: &'a Value, key: &str) -> Option<&'a str> {
    p.get(key).and_then(Value::as_str)
}

/// `{op: "project"|"stamp"|"graduate"|"set_expected"|"waves", ...}`.
/// `project` reads the graph this keeper owns and rewrites each named node's
/// linked plan doc; `stamp`/`graduate`/`set_expected` write plan frontmatter
/// with the same exit codes the Python module returned; `waves` derives an
/// epic's wave strata. Plan-doc events ride the keeper's own `--events`
/// journal when one is configured.
pub(crate) fn handle_plan_docs(state: &StoreState, params: &Value) -> Result<Value, StoreError> {
    let op = opt_str(params, "op").unwrap_or_default();
    match op {
        "project" => {
            let ids: Vec<String> = match params.get("ids").and_then(Value::as_array) {
                Some(a) => a
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect(),
                None => return Err(StoreError::Invalid("plan_docs project needs ids".into())),
            };
            if ids.is_empty() {
                return Ok(json!({"rewritten": 0, "warnings": []}));
            }
            let cached = cached_entries(state, false, false)?;
            let root = opt_str(params, "root").map(str::to_string);
            let pair = |key: &str| -> Option<(String, Vec<String>)> {
                params.get(key).and_then(|p| {
                    let id = p.get("id").and_then(Value::as_str)?.to_string();
                    let keys: Vec<String> = p
                        .get("keys")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default();
                    Some((id, keys))
                })
            };
            let (rewritten, warnings) = super::project::project_graph_nodes(
                &cached,
                &ids,
                root.as_deref(),
                pair("mirror_keys_for"),
                opt_str(params, "force_status_off_terminal_for").map(str::to_string),
                pair("clear_keys_for"),
            );
            Ok(json!({"rewritten": rewritten, "warnings": warnings}))
        }
        "stamp" => {
            let Some(plan_path) = opt_str(params, "plan_path") else {
                return Err(StoreError::Invalid(
                    "plan_docs stamp needs plan_path".into(),
                ));
            };
            let session_id = opt_str(params, "session_id").unwrap_or_default();
            let urls: Vec<String> = params
                .get("urls")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let result = super::stamp::cmd_stamp(
                Path::new(plan_path),
                session_id,
                &urls,
                params
                    .get("expected_url_count")
                    .and_then(Value::as_u64)
                    .map(|n| n as u32),
                params
                    .get("dry_run")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                state.events.as_deref(),
            );
            Ok(json!({"exit": result.exit, "message": result.message}))
        }
        "graduate" => {
            let Some(plan_path) = opt_str(params, "plan_path") else {
                return Err(StoreError::Invalid(
                    "plan_docs graduate needs plan_path".into(),
                ));
            };
            let result = super::stamp::cmd_graduate(
                Path::new(plan_path),
                params
                    .get("dry_run")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                state.events.as_deref(),
            );
            Ok(json!({"exit": result.exit, "message": result.message}))
        }
        "set_expected" => {
            let Some(plan_path) = opt_str(params, "plan_path") else {
                return Err(StoreError::Invalid(
                    "plan_docs set_expected needs plan_path".into(),
                ));
            };
            let count = params.get("count").and_then(Value::as_u64).unwrap_or(0) as u32;
            let result = super::stamp::cmd_set_expected(
                Path::new(plan_path),
                count,
                params
                    .get("dry_run")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            );
            Ok(json!({"exit": result.exit, "message": result.message}))
        }
        "waves" => {
            let Some(epic_id) = opt_str(params, "epic_id") else {
                return Err(StoreError::Invalid("plan_docs waves needs epic_id".into()));
            };
            let cached = cached_entries(state, false, false)?;
            let (wave_by_id, max_wave) = super::rollup::compute_waves(epic_id, &cached);
            let wave_by_id: Map<String, Value> = wave_by_id
                .into_iter()
                .map(|(k, v)| (k, Value::Number(v.into())))
                .collect();
            Ok(json!({"wave_by_id": wave_by_id, "max_wave": max_wave}))
        }
        other => Err(StoreError::Invalid(format!(
            "unknown plan_docs op {other:?}"
        ))),
    }
}

//! The keeper's `plan_docs` method: the bridge between the store method table
//! and the plan-doc writer, so the Python callers are clients and no second
//! writer leg exists.

use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::graph_keeper::{cached_entries, StoreState};
use crate::graph_store::StoreError;

fn opt_str<'a>(p: &'a Value, key: &str) -> Option<&'a str> {
    p.get(key).and_then(Value::as_str)
}

/// The caller's project journal when it names one, else the keeper's own.
fn events_path<'a>(state: &'a StoreState, p: &'a Value) -> Option<&'a Path> {
    opt_str(p, "events_path")
        .map(Path::new)
        .or(state.events.as_deref())
}

/// The keeper runs in its own cwd, so a relative path means the caller's.
fn caller_path(p: &Value, raw: &str) -> PathBuf {
    match opt_str(p, "cwd") {
        Some(cwd) => Path::new(cwd).join(raw),
        None => PathBuf::from(raw),
    }
}

/// `{op: "project"|"stamp"|"graduate"|"set_expected"|"waves", ...}`.
/// `project` reads the graph this keeper owns and rewrites each named node's
/// linked plan doc; `stamp`/`graduate`/`set_expected` write plan frontmatter
/// with the same exit codes the Python module returned; `waves` derives an
/// epic's wave strata. Plan-doc events go to the caller's `events_path`,
/// else the keeper's own `--events` journal.
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
            // No root: a relative plan_path resolves against the caller's
            // canonical checkout, as the Python converger's lazy repo_root did.
            let root = opt_str(params, "root").map(str::to_string).or_else(|| {
                opt_str(params, "cwd").map(|cwd| {
                    crate::paths::canonical_repo_root(Path::new(cwd))
                        .unwrap_or_else(|| PathBuf::from(cwd))
                        .to_string_lossy()
                        .into_owned()
                })
            });
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
                &caller_path(params, plan_path),
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
                events_path(state, params),
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
                &caller_path(params, plan_path),
                params
                    .get("dry_run")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                events_path(state, params),
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
                &caller_path(params, plan_path),
                count,
                params
                    .get("dry_run")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            );
            // Exit 3 is a missing doc: benign for decompose, which cannot
            // stamp it at ship either.
            let status = match result.exit {
                0 => "ok",
                3 => "skipped",
                _ => "failed",
            };
            Ok(json!({"exit": result.exit, "message": result.message, "status": status}))
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
        // `fno do plan stamp|graduate|set-expected` hand their raw flags here.
        "argv" => match argv_params(params) {
            Ok(parsed) => handle_plan_docs(state, &parsed),
            Err(message) => Ok(json!({"exit": 2, "message": message})),
        },
        other => Err(StoreError::Invalid(format!(
            "unknown plan_docs op {other:?}"
        ))),
    }
}

/// Parse the retired stamp module's command line into op params, keeping the
/// caller's `cwd` and `events_path`.
fn argv_params(params: &Value) -> Result<Value, String> {
    let verb = opt_str(params, "verb").unwrap_or_default();
    let args: Vec<&str> = params
        .get("args")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut out = Map::new();
    for key in ["cwd", "events_path"] {
        if let Some(v) = params.get(key) {
            out.insert(key.into(), v.clone());
        }
    }
    let mut urls = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let flag = args[i];
        if flag == "--dry-run" {
            out.insert("dry_run".into(), Value::Bool(true));
            i += 1;
            continue;
        }
        let value = args
            .get(i + 1)
            .ok_or_else(|| format!("error: {flag} needs a value"))?;
        let number = || {
            value
                .parse::<u64>()
                .map(Value::from)
                .map_err(|_| format!("error: {flag} needs an integer, got {value:?}"))
        };
        match flag {
            "--plan-path" => out.insert("plan_path".into(), Value::from(*value)),
            "--session-id" => out.insert("session_id".into(), Value::from(*value)),
            "--url" => {
                urls.push(Value::from(*value));
                None
            }
            "--expected-url-count" => out.insert("expected_url_count".into(), number()?),
            "--count" => out.insert("count".into(), number()?),
            _ => return Err(format!("error: unknown {verb} flag {flag:?}")),
        };
        i += 2;
    }
    if !out.contains_key("plan_path") {
        return Err(format!("error: {verb} needs --plan-path"));
    }
    let op = match verb {
        "stamp" => "stamp",
        "graduate" => "graduate",
        "set-expected" => "set_expected",
        _ => return Err(format!("error: unknown plan verb {verb:?}")),
    };
    out.insert("op".into(), Value::from(op));
    out.insert("urls".into(), Value::Array(urls));
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv_parses_the_stamp_command_line() {
        let parsed = argv_params(&json!({
            "verb": "stamp",
            "cwd": "/w",
            "args": ["--plan-path", "p.md", "--session-id", "s", "--url", "u1",
                     "--url", "u2", "--expected-url-count", "2", "--dry-run"],
        }))
        .unwrap();
        assert_eq!(parsed["op"], "stamp");
        assert_eq!(parsed["plan_path"], "p.md");
        assert_eq!(parsed["urls"], json!(["u1", "u2"]));
        assert_eq!(parsed["expected_url_count"], 2);
        assert_eq!(parsed["dry_run"], true);
        assert_eq!(parsed["cwd"], "/w");
        assert_eq!(caller_path(&parsed, "p.md"), PathBuf::from("/w/p.md"));
    }

    #[test]
    fn argv_refuses_bad_input() {
        let bad = |verb: &str, args: Value| argv_params(&json!({"verb": verb, "args": args}));
        assert!(bad("stamp", json!(["--session-id", "s"])).is_err());
        assert!(bad("set-expected", json!(["--plan-path", "p", "--count", "x"])).is_err());
        assert!(bad("stamp", json!(["--plan-path"])).is_err());
        assert!(bad("stamp", json!(["--plan-path", "p", "--bogus", "1"])).is_err());
        assert!(bad("nope", json!(["--plan-path", "p"])).is_err());
    }
}

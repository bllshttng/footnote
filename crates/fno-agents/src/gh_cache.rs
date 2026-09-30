//! Permanent rows for GitHub facts that do not change: merged-PR facts
//! and repo metadata (the default branch). The `gh-cache` verb is
//! transport-only (a client.rs early arm, like pr-worktree): JSON request
//! on stdin, one row JSON on stdout. Python callers reach it through
//! verb_call, which keeps cli/src/fno at glue. A missing or corrupt row is
//! a miss, never an error: the network read stays the truth.

use serde_json::{json, Map, Value};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn rows_root(cwd: &Path) -> Option<PathBuf> {
    let env = std::env::var("FNO_GH_FACTS_DIR")
        .ok()
        .filter(|v| !v.is_empty());
    match env {
        Some(dir) => Some(PathBuf::from(dir)),
        None => crate::agents_config::state_dir(cwd).map(|root| root.join("cache/gh-facts")),
    }
}

fn row_path(root: &Path, kind: &str, slug: &str, pr: Option<u64>) -> Option<PathBuf> {
    let key = slug.trim().trim_end_matches(".git").replace('/', "--");
    if key.is_empty() || kind.is_empty() || kind.contains("..") || kind.contains('/') {
        return None;
    }
    let name = match pr {
        Some(n) => format!("{key}-pr{n}.json"),
        None => format!("{key}.json"),
    };
    Some(root.join(kind).join(name))
}

fn read_row(
    root: &Path,
    kind: &str,
    slug: &str,
    pr: Option<u64>,
    ttl_s: Option<u64>,
    now: f64,
) -> Value {
    let miss = json!({"row": null});
    let Some(path) = row_path(root, kind, slug, pr) else {
        return miss;
    };
    let Some(row) = fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .filter(|row| row.is_object())
    else {
        return miss;
    };
    if let Some(ttl) = ttl_s {
        let ts = row.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
        if ts <= 0.0 || now - ts >= ttl as f64 {
            return miss;
        }
    }
    json!({"row": row})
}

fn write_row(root: &Path, kind: &str, slug: &str, pr: Option<u64>, row: &Value) -> bool {
    let Some(path) = row_path(root, kind, slug, pr) else {
        return false;
    };
    let Value::Object(map) = row else {
        return false;
    };
    let mut out = Map::new();
    for (k, v) in map {
        out.insert(k.clone(), v.clone());
    }
    // The freshness stamp lands last so a caller-supplied ts never wins.
    out.insert("ts".to_string(), json!(now_secs()));
    let text = Value::Object(out).to_string();
    if fs::create_dir_all(path.parent().unwrap_or(root)).is_err() {
        return false;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, text).is_ok() && fs::rename(&tmp, &path).is_ok()
}

pub fn run() -> i32 {
    let mut input = String::new();
    if io::stdin().read_to_string(&mut input).is_err() {
        return 2;
    }
    let payload: Value = match serde_json::from_str(&input) {
        Ok(value) => value,
        Err(_) => return 2,
    };
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let Some(root) = rows_root(&cwd) else {
        println!("{}", json!({"row": null}));
        return 0;
    };
    let slug = payload.get("slug").and_then(Value::as_str).unwrap_or("");
    let pr = payload.get("pr").and_then(Value::as_u64);
    let kind = payload.get("kind").and_then(Value::as_str).unwrap_or("");
    match payload.get("op").and_then(Value::as_str) {
        Some("read") => {
            let ttl = payload.get("ttl_s").and_then(Value::as_u64);
            println!("{}", read_row(&root, kind, slug, pr, ttl, now_secs()));
            0
        }
        Some("write") => {
            let Some(row) = payload.get("row").filter(|r| r.is_object()) else {
                return 2;
            };
            let ok = write_row(&root, kind, slug, pr, row);
            println!("{}", json!({"written": ok}));
            if ok {
                0
            } else {
                1
            }
        }
        _ => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn rows_round_trip_per_kind_and_slug_and_honor_ttl() {
        let dir = tempdir().unwrap();
        assert_eq!(
            read_row(dir.path(), "merged", "o/r", Some(7), None, 100.0)["row"],
            Value::Null
        );
        assert!(write_row(
            dir.path(),
            "merged",
            "o/r",
            Some(7),
            &json!({"info": {"state": "MERGED"}})
        ));
        assert_eq!(
            read_row(dir.path(), "merged", "o/r", Some(7), None, 101.0)["row"]["info"]["state"],
            json!("MERGED")
        );
        assert_eq!(
            read_row(dir.path(), "merged", "o/r", Some(7), Some(60), 1e9)["row"],
            Value::Null
        );
        assert_eq!(
            read_row(dir.path(), "merged", "other", Some(7), None, 101.0)["row"],
            Value::Null
        );
        assert!(write_row(
            dir.path(),
            "repo-meta",
            "o/r",
            None,
            &json!({"default_branch": "main"})
        ));
        assert_eq!(
            read_row(dir.path(), "repo-meta", "o/r", None, None, 101.0)["row"]["default_branch"],
            json!("main")
        );
        assert_eq!(
            read_row(dir.path(), "repo-meta", "o/x", None, None, 101.0)["row"],
            Value::Null
        );
    }
}

use serde_json::Value;
use std::ffi::{OsStr, OsString};
use std::process::Command;
use std::time::Instant;

#[derive(Debug, Clone, Copy)]
pub enum Kind {
    Next,
    Undispatched,
    Held,
}

#[derive(Debug, serde::Serialize)]
pub struct Receipt {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bound_s: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Next => "next",
        Kind::Undispatched => "undispatched",
        Kind::Held => "held",
    }
}

fn project_name(args: &[String]) -> &str {
    args.windows(2)
        .find(|pair| pair[0] == "--project")
        .map(|pair| pair[1].as_str())
        .unwrap_or("-")
}

fn receipt(
    status: &str,
    answer: Option<Value>,
    bound_s: u64,
    elapsed_ms: u128,
    reason: Option<&str>,
    detail: Option<String>,
) -> Receipt {
    Receipt {
        status: status.to_string(),
        answer,
        bound_s: Some(bound_s),
        elapsed_ms: Some(elapsed_ms),
        reason: reason.map(str::to_string),
        detail,
    }
}

fn head(text: &[u8], limit: usize) -> String {
    String::from_utf8_lossy(text).chars().take(limit).collect()
}

/// The `project=<p>` token the unmeasured detail carries; arm_repair parses
/// it back out of the journal row, so writer and readers share this one name.
pub(crate) const PROJECT_TOKEN: &str = "project=";

pub(crate) fn unmeasured_detail(
    kind: Kind,
    args: &[String],
    bound_s: u64,
    suffix: Option<&str>,
) -> String {
    let mut detail = format!(
        "{PROJECT_TOKEN}{} bound={}s: fno backlog {} did not answer inside its {}s budget; the arm_watch heal lane retries it",
        project_name(args),
        bound_s,
        kind_name(kind),
        bound_s
    );
    if let Some(suffix) = suffix.filter(|s| !s.is_empty()) {
        detail.push_str("; ");
        detail.push_str(suffix);
    }
    detail
}

fn enrich_next(mut node: Value, fno_py: &OsStr) -> Result<Value, String> {
    let Some(object) = node.as_object_mut() else {
        return Err("fno backlog next returned an unexpected shape".to_string());
    };
    let Some(id) = object
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return Err("fno backlog next returned an unexpected shape".to_string());
    };
    if object
        .get("_resolved_cwd")
        .and_then(Value::as_str)
        .is_some_and(|cwd| !cwd.is_empty())
    {
        return Ok(node);
    }
    let output = crate::bounded_cmd::output_with_timeout_result(
        {
            let mut command = Command::new(fno_py);
            command.args(["backlog", "get", id]);
            command
        },
        30,
    );
    let Ok(output) = output else {
        return Ok(node);
    };
    if !output.status.success() {
        return Ok(node);
    }
    let Ok(full) = serde_json::from_slice::<Value>(&output.stdout) else {
        return Ok(node);
    };
    if let Some(cwd) = full.get("_resolved_cwd").and_then(Value::as_str) {
        object.insert("_resolved_cwd".to_string(), Value::String(cwd.to_string()));
    }
    Ok(node)
}

pub fn select_read(kind: Kind, args: &[String], fno_py: &OsStr, bound_s: u64) -> Receipt {
    let started = Instant::now();
    let mut cmd = Command::new(fno_py);
    match kind {
        Kind::Next => {
            cmd.args(["backlog", "next"]);
        }
        Kind::Undispatched => {
            cmd.args(["backlog", "undispatched", "--json"]);
        }
        // Held never spawns: run() answers it from the journals directly.
        Kind::Held => {}
    }
    cmd.args(args);
    let output = match crate::bounded_cmd::output_with_timeout_result(cmd, bound_s) {
        Ok(output) => output,
        Err(error) => {
            return receipt(
                "error",
                None,
                bound_s,
                started.elapsed().as_millis(),
                Some("next-error"),
                Some(format!(
                    "fno backlog {} failed to start: {}",
                    kind_name(kind),
                    error
                )),
            )
        }
    };
    let elapsed_ms = started.elapsed().as_millis();
    if output.status.code().is_none() {
        if elapsed_ms >= u128::from(bound_s) * 1_000 {
            return receipt(
                "unmeasured",
                None,
                bound_s,
                elapsed_ms,
                Some("select-unmeasured"),
                Some(unmeasured_detail(kind, args, bound_s, None)),
            );
        }
        return receipt(
            "error",
            None,
            bound_s,
            elapsed_ms,
            Some("next-error"),
            Some(format!(
                "fno backlog {} terminated before its {}s budget",
                kind_name(kind),
                bound_s
            )),
        );
    }
    if !output.status.success() {
        let stderr_full = String::from_utf8_lossy(&output.stderr);
        let stderr = head(&output.stderr, 160);
        let transient = stderr_full.contains("store keeper unavailable")
            || stderr_full.contains("claim state is unavailable");
        return if transient {
            receipt(
                "unmeasured",
                None,
                bound_s,
                elapsed_ms,
                Some("select-unmeasured"),
                Some(unmeasured_detail(kind, args, bound_s, Some(&stderr))),
            )
        } else {
            receipt(
                "error",
                None,
                bound_s,
                elapsed_ms,
                Some("next-error"),
                Some(format!(
                    "fno backlog {} exited {}: {}",
                    kind_name(kind),
                    output.status.code().unwrap_or(-1),
                    stderr
                )),
            )
        };
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        return receipt(
            "error",
            None,
            bound_s,
            elapsed_ms,
            Some("next-error"),
            Some(format!(
                "fno backlog {} returned empty output",
                kind_name(kind)
            )),
        );
    }
    let parsed = match serde_json::from_str::<Value>(&stdout) {
        Ok(value) => value,
        Err(_error) => {
            return receipt(
                "error",
                None,
                bound_s,
                elapsed_ms,
                Some("next-error"),
                Some(format!(
                    "fno backlog {} returned invalid JSON: {}",
                    kind_name(kind),
                    head(stdout.as_bytes(), 200)
                )),
            )
        }
    };
    let answer = match kind {
        Kind::Next if parsed.is_null() => parsed,
        Kind::Next => match enrich_next(parsed, fno_py) {
            Ok(node) => node,
            Err(detail) => {
                return receipt(
                    "error",
                    None,
                    bound_s,
                    elapsed_ms,
                    Some("next-error"),
                    Some(detail),
                )
            }
        },
        Kind::Undispatched | Kind::Held => parsed,
    };
    receipt("ok", Some(answer), bound_s, elapsed_ms, None, None)
}

/// The `held` kind answers from the question journals directly - no fno-py
/// cold start, no bound to ride out. The fno dir is the one holding
/// `graph_json_path(cwd)` (king_board/scope.rs), so the map the Python guard
/// reads is the same fold `needs::held_map` gives the keeper.
fn run_held(bound_s: u64) -> i32 {
    let started = Instant::now();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let fno_dir = crate::king_board::graph_json_path(&cwd)
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let answer = crate::needs::held_map(&fno_dir, &cwd);
    match serde_json::to_value(&answer) {
        Ok(answer) => {
            let receipt = receipt(
                "ok",
                Some(answer),
                bound_s,
                started.elapsed().as_millis(),
                None,
                None,
            );
            match serde_json::to_string(&receipt) {
                Ok(json) => {
                    println!("{json}");
                    0
                }
                Err(error) => {
                    eprintln!("select-read: failed to encode receipt: {error}");
                    1
                }
            }
        }
        Err(error) => {
            eprintln!("select-read held: failed to encode answer: {error}");
            1
        }
    }
}

fn usage() {
    eprintln!("usage: fno-agents select-read <next|undispatched|held> [--project P] [--mission M]");
}

pub fn run(args: &[String]) -> i32 {
    let kind = match args.first().map(String::as_str) {
        Some("next") => Kind::Next,
        Some("undispatched") => Kind::Undispatched,
        Some("held") => {
            let bound_s = crate::agents_config::auto_continue_select_timeout_s(
                &std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            );
            return run_held(bound_s);
        }
        _ => {
            usage();
            return 2;
        }
    };
    let mut forwarded = Vec::new();
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--project" | "--mission" => {
                let Some(value) = args.get(index + 1) else {
                    usage();
                    return 2;
                };
                forwarded.push(args[index].clone());
                forwarded.push(value.clone());
                index += 2;
            }
            _ => {
                usage();
                return 2;
            }
        }
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let bound_s = crate::agents_config::auto_continue_select_timeout_s(&cwd);
    let fno_py: OsString = crate::scrape::fno_py();
    let receipt = select_read(kind, &forwarded, &fno_py, bound_s);
    match serde_json::to_string(&receipt) {
        Ok(json) => {
            println!("{json}");
            0
        }
        Err(error) => {
            eprintln!("select-read: failed to encode receipt: {error}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn stub(body: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        crate::write_exec_stub(dir.path(), "fno-py", body);
        dir
    }

    #[test]
    fn next_happy_path_enriches_resolved_cwd() {
        let dir = stub(
            "#!/bin/sh\ncase \"$2 $3\" in\n  next*) printf '%s' '{\"id\":\"x-1\",\"cwd\":\"/raw\"}' ;;\n  get*) printf '%s' '{\"id\":\"x-1\",\"_resolved_cwd\":\"/mapped\"}' ;;\nesac\n",
        );
        let args = vec!["--project".to_string(), "fno".to_string()];
        let fno_py = dir.path().join("fno-py");
        let receipt = select_read(Kind::Next, &args, fno_py.as_os_str(), 5);
        assert_eq!(receipt.status, "ok");
        assert_eq!(receipt.bound_s, Some(5));
        assert_eq!(receipt.answer.unwrap()["_resolved_cwd"], "/mapped");
    }

    #[test]
    fn next_get_failure_is_nonfatal() {
        let dir = stub(
            "#!/bin/sh\ncase \"$2 $3\" in\n  next*) printf '%s' '{\"id\":\"x-1\",\"cwd\":\"/raw\"}' ;;\n  get*) printf '%s' 'get exploded' >&2; exit 1 ;;\nesac\n",
        );
        let args = vec!["--project".to_string(), "fno".to_string()];
        let fno_py = dir.path().join("fno-py");
        let receipt = select_read(Kind::Next, &args, fno_py.as_os_str(), 5);
        let answer = receipt.answer.unwrap();
        assert_eq!(receipt.status, "ok");
        assert_eq!(answer["id"], "x-1");
        assert!(answer.get("_resolved_cwd").is_none());
    }

    #[test]
    fn next_skips_get_when_already_resolved() {
        let dir = stub(
            "#!/bin/sh\ncase \"$2 $3\" in\n  next*) printf '%s' '{\"id\":\"x-1\",\"cwd\":\"/raw\",\"_resolved_cwd\":\"/already\"}' ;;\n  get*) exit 42 ;;\nesac\n",
        );
        let args = vec!["--project".to_string(), "fno".to_string()];
        let fno_py = dir.path().join("fno-py");
        let receipt = select_read(Kind::Next, &args, fno_py.as_os_str(), 5);
        assert_eq!(receipt.status, "ok");
        assert_eq!(receipt.answer.unwrap()["_resolved_cwd"], "/already");
    }

    #[test]
    fn stalled_selection_is_unmeasured_at_the_bound() {
        // No exec: sleep is a grandchild holding the piped stdout, the shape
        // a wrapper-script stand-in produces. The bound must still hold.
        let dir = stub("#!/bin/sh\nsleep 3\n");
        let started = Instant::now();
        let args = vec!["--project".to_string(), "fno".to_string()];
        let fno_py = dir.path().join("fno-py");
        let receipt = select_read(Kind::Next, &args, fno_py.as_os_str(), 1);
        assert!(started.elapsed().as_secs_f32() < 2.5);
        assert_eq!(receipt.status, "unmeasured");
        assert_eq!(receipt.reason.as_deref(), Some("select-unmeasured"));
        assert!(receipt
            .detail
            .as_deref()
            .unwrap()
            .contains("project=fno bound=1s"));
    }

    #[test]
    fn signal_termination_before_the_bound_is_an_error() {
        let dir = stub("#!/bin/sh\nkill -TERM $$\n");
        let args = vec!["--project".to_string(), "fno".to_string()];
        let fno_py = dir.path().join("fno-py");
        let receipt = select_read(Kind::Next, &args, fno_py.as_os_str(), 5);
        assert_eq!(receipt.status, "error");
        assert_eq!(receipt.reason.as_deref(), Some("next-error"));
    }

    #[test]
    fn transient_store_failure_is_unmeasured_but_other_failure_is_error() {
        let transient = stub(
            "#!/bin/sh\nprintf '%*s' 200 '' | tr ' ' x >&2\nprintf '%s' ' store keeper unavailable; selection refused: stalled' >&2\nexit 1\n",
        );
        let args = vec!["--project".to_string(), "fno".to_string()];
        let fno_py = transient.path().join("fno-py");
        let receipt = select_read(Kind::Next, &args, fno_py.as_os_str(), 5);
        assert_eq!(receipt.status, "unmeasured");
        assert_eq!(receipt.reason.as_deref(), Some("select-unmeasured"));

        let ordinary = stub("#!/bin/sh\nprintf '%s' 'Error: graph unreadable' >&2\nexit 1\n");
        let fno_py = ordinary.path().join("fno-py");
        let receipt = select_read(Kind::Next, &args, fno_py.as_os_str(), 5);
        assert_eq!(receipt.status, "error");
        assert_eq!(receipt.reason.as_deref(), Some("next-error"));
    }
}

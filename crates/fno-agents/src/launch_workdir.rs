//! The spawn door's launch-workdir resolution: JSON payload in, one JSON
//! answer out. Ported from `node_dispatch._worktree_ensure_for_launch`
//! (Python), with the one behavior change the port exists for: the worktree
//! ensure runs with `--name <node>`, so a node-seeded spawn lands in the
//! node's existing tree instead of minting a worker-named tree beside it.
//! A `hold` answer is a VALID answer at exit 0 - the caller prints the hold
//! line and keeps the node claimable; exit 2 here means transport failure
//! only (unreadable stdin, bad JSON), the same contract `spawn-axes` uses.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

const GIT_TIMEOUT: Duration = Duration::from_secs(10);
const ENSURE_TIMEOUT: Duration = Duration::from_secs(120);

pub fn run_launch_workdir(args: &[String]) -> i32 {
    use std::io::Read;

    let mut payload = String::new();
    let read = if let Some(path) = args.iter().find_map(|a| a.strip_prefix("--payload-file=")) {
        std::fs::read_to_string(path)
    } else {
        std::io::stdin()
            .read_to_string(&mut payload)
            .map(|_| payload.clone())
    };
    let payload = match read {
        Ok(text) => text,
        Err(e) => {
            eprint!("launch-workdir: cannot read payload: {e}\n");
            return 2;
        }
    };
    let parsed: Value = match serde_json::from_str(&payload) {
        Ok(v) => v,
        Err(e) => {
            eprint!("launch-workdir: bad payload: {e}\n");
            return 2;
        }
    };
    let (answer, receipt) = decide(&parsed);
    if let Some(line) = receipt {
        eprint!("{line}\n");
    }
    println!("{answer}");
    0
}

/// The port of `_worktree_ensure_for_launch`, answers in its order:
/// a missing recorded cwd passes through verbatim (the spawn surfaces its
/// own error), a non-git recorded cwd launches in place (a vault project,
/// `worktree.policy = never` by design), any other git failure holds, and
/// an ensure refusal or empty answer holds rather than launching on
/// canonical main.
fn decide(payload: &Value) -> (Value, Option<String>) {
    let recorded = payload
        .get("recorded_cwd")
        .and_then(Value::as_str)
        .unwrap_or("");
    let node = payload.get("node").and_then(Value::as_str).unwrap_or("");
    let harness = payload.get("harness").and_then(Value::as_str).unwrap_or("");
    let cwd = Path::new(if recorded.is_empty() { "." } else { recorded });

    let git_cmd = vec![
        "git".to_string(),
        "-C".to_string(),
        cwd.to_string_lossy().into_owned(),
        "rev-parse".to_string(),
        "--show-toplevel".to_string(),
    ];
    let repo = crate::king_board::budget::run_with_timeout(&git_cmd, Path::new("."), GIT_TIMEOUT);

    if !cwd.is_dir() {
        return (json!({ "workdir": recorded }), None);
    }
    let top = match repo {
        Ok(out) => String::from_utf8_lossy(&out).trim().to_string(),
        Err(e) => {
            // ONLY a genuine "not a git repository" answer means
            // launch-in-place; any other git failure (dubious ownership, a
            // corrupted .git) must HOLD, not silently fall back to the
            // canonical checkout this verb exists to keep workers off.
            if e.message().contains("not a git repository") {
                return (json!({ "workdir": recorded }), None);
            }
            return (json!({ "hold": e.message() }), None);
        }
    };
    if top.is_empty() {
        return (
            json!({ "hold": "git rev-parse printed no repository toplevel" }),
            None,
        );
    }

    let mut cmd = crate::king_board::budget::fno_py_cmd();
    cmd.extend([
        "workspace".to_string(),
        "worktree".to_string(),
        "ensure".to_string(),
        "--repo".to_string(),
        top,
        "--name".to_string(),
        node.to_string(),
        "--harness".to_string(),
        harness.to_string(),
    ]);
    match crate::king_board::budget::run_with_timeout_full(&cmd, Path::new("."), ENSURE_TIMEOUT) {
        Err(e) => (json!({ "hold": e.message() }), None),
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stdout.is_empty() {
                return (json!({ "hold": "worktree ensure printed no path" }), None);
            }
            let receipt = if stderr.contains("created=false") {
                Some(format!(
                    "fno agents spawn: resuming {node} in its existing worktree {stdout}"
                ))
            } else {
                None
            };
            (json!({ "workdir": stdout }), receipt)
        }
    }
}

/// The typed answer the hosted Codex thread lane reads. The same `decide`
/// contract unwrapped: the node's worktree path, or the hold reason.
pub(crate) fn ensure_node_workdir(
    cwd: &Path,
    node: &str,
    harness: &str,
) -> Result<PathBuf, String> {
    let payload = json!({
        "recorded_cwd": cwd.to_string_lossy(),
        "node": node,
        "harness": harness,
    });
    let (answer, _receipt) = decide(&payload);
    if let Some(hold) = answer.get("hold").and_then(Value::as_str) {
        return Err(hold.to_string());
    }
    match answer.get("workdir").and_then(Value::as_str) {
        Some(dir) if !dir.is_empty() => Ok(PathBuf::from(dir)),
        _ => Err("launch-workdir answered neither a workdir nor a hold".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decide_with(repo_exit: i32, non_git: bool, ensure_stdout: &str) -> (Value, Option<String>) {
        // Drives `decide` under the crate's env lock (set_var races across
        // parallel tests) over a temp git repo with a fake `fno-py` honored
        // through FNO_PY (the fno_py_cmd resolution).
        let _env = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        let bin = dir.path().join("fno-py");
        std::fs::write(
            &bin,
            format!("#!/bin/sh\ncat >/dev/null\necho '{ensure_stdout}'\necho 'worktree ensure: reusing ... created=false' >&2\nexit {}\n", repo_exit),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = std::fs::metadata(&bin).unwrap().permissions();
            perm.set_mode(0o755);
            std::fs::set_permissions(&bin, perm).unwrap();
        }
        std::env::set_var("FNO_PY", &bin);
        let plain = tempfile::tempdir().unwrap();
        let cwd = if non_git {
            plain.path().to_path_buf()
        } else {
            dir.path().to_path_buf()
        };
        let payload = json!({
            "recorded_cwd": cwd.to_string_lossy(),
            "node": "x-eeee",
            "harness": "claude",
        });
        // `plain` stays bound until decide returns, so the non-git cwd the
        // git call reads is still on disk.
        decide(&payload)
    }

    #[test]
    fn a_missing_recorded_cwd_passes_through_verbatim() {
        let (answer, receipt) = decide(&json!({
            "recorded_cwd": "/nonexistent/x-3333-scratch",
            "node": "x-1",
            "harness": "claude",
        }));
        assert_eq!(answer, json!({ "workdir": "/nonexistent/x-3333-scratch" }));
        assert!(receipt.is_none());
    }

    #[test]
    fn a_non_git_recorded_cwd_launches_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let (answer, receipt) = decide(&json!({
            "recorded_cwd": dir.path().to_string_lossy(),
            "node": "x-1",
            "harness": "claude",
        }));
        assert_eq!(answer, json!({ "workdir": dir.path().to_string_lossy() }));
        assert!(receipt.is_none());
    }

    #[test]
    fn an_ensure_refusal_holds() {
        let (answer, receipt) = decide_with(1, false, "");
        assert!(answer.get("hold").is_some(), "{answer}");
        assert!(receipt.is_none());
    }

    #[test]
    fn an_ensure_success_answers_the_path_and_names_the_resume() {
        let (answer, receipt) = decide_with(0, false, "/wt/x-eeee");
        assert_eq!(answer, json!({ "workdir": "/wt/x-eeee" }));
        assert_eq!(
            receipt.as_deref(),
            Some("fno agents spawn: resuming x-eeee in its existing worktree /wt/x-eeee")
        );
    }
}

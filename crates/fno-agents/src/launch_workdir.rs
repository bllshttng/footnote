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
    let branch = payload.get("branch").and_then(Value::as_str).unwrap_or("");
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
    if !branch.is_empty() {
        cmd.extend(["--branch".to_string(), branch.to_string()]);
    }
    match crate::king_board::budget::run_with_timeout_full(&cmd, Path::new("."), ENSURE_TIMEOUT) {
        Err(e) => (json!({ "hold": e.message() }), None),
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stdout.is_empty() {
                return (json!({ "hold": "worktree ensure printed no path" }), None);
            }
            // The venv guard runs before any worker lands: a tree whose venv
            // aliases the canonical checkout rewrites the canonical venv's
            // scripts on its first install, and those scripts die with this
            // tree (x-0242). Holding is the refusal the accident never gets
            // past.
            let worktree = PathBuf::from(&stdout);
            if let Some(risk) = canonical_venv_risk(
                &worktree,
                crate::paths::canonical_repo_root(&worktree).as_deref(),
                std::env::var_os("UV_PROJECT_ENVIRONMENT").as_deref(),
            ) {
                return (json!({ "hold": risk }), None);
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

/// The worktree-venv guard: a worktree session must never install into the
/// CANONICAL checkout's `cli/.venv`. Two aliasing mechanisms produced x-0242:
/// the worktree's `cli/.venv` is a symlink resolving into the canonical
/// checkout, or `UV_PROJECT_ENVIRONMENT` points there - an `uv sync` under
/// either rewrites the canonical venv's console scripts with this worktree's
/// interpreter, and every deployed script dies when the worktree is pruned.
/// `canonical` is the main checkout behind `worktree` (None when `worktree`
/// sits outside a repo); a canonical `worktree` passes, since an install
/// there belongs there. `Some` is the hold reason.
fn canonical_venv_risk(
    worktree: &Path,
    canonical: Option<&Path>,
    uv_env: Option<&std::ffi::OsStr>,
) -> Option<String> {
    let canonical = canonical?;
    if same_root(worktree, canonical) {
        return None;
    }
    if let Ok(target) = std::fs::read_link(worktree.join("cli/.venv")) {
        if path_inside(&target, canonical) {
            return Some(format!(
                "worktree {} symlinks cli/.venv into the canonical checkout \
                 ({}); an install here rewrites the canonical venv's scripts \
                 with this worktree's python, and they break when this \
                 worktree is pruned. Give the worktree its own cli/.venv.",
                worktree.display(),
                target.display()
            ));
        }
    }
    if let Some(env) = uv_env.filter(|v| !v.is_empty()) {
        if path_inside(&uv_env_path(env), canonical) {
            return Some(format!(
                "UV_PROJECT_ENVIRONMENT points into the canonical checkout; \
                 an install from worktree {} would rewrite the canonical cli \
                 venv's scripts with this worktree's python. Unset it or \
                 repoint it inside the worktree.",
                worktree.display()
            ));
        }
    }
    None
}

/// Equal lexically or through symlinks: a worktree and its canonical root may
/// be spelled on either side of a `/tmp` vs `/private/tmp` alias.
fn same_root(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// `p` sits under `root`, lexically first, then through symlinks. A path that
/// cannot canonicalize is judged lexically only.
fn path_inside(p: &Path, root: &Path) -> bool {
    if p.starts_with(root) {
        return true;
    }
    match (std::fs::canonicalize(p), std::fs::canonicalize(root)) {
        (Ok(p), Ok(root)) => p.starts_with(root),
        _ => false,
    }
}

/// `~/...` in an env var expands against `$HOME` like every sibling reader;
/// anything else passes through verbatim.
fn uv_env_path(env: &std::ffi::OsStr) -> PathBuf {
    let text = env.to_string_lossy();
    match text.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest),
            None => PathBuf::from(text.into_owned()),
        },
        None => PathBuf::from(text.into_owned()),
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
        let bin = crate::write_exec_stub(
            dir.path(),
            "fno-py",
            &format!("#!/bin/sh\ncat >/dev/null\necho '{ensure_stdout}'\necho 'worktree ensure: reusing ... created=false' >&2\nexit {}\n", repo_exit),
        );
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
        // The venv guard rides the same success contract: a clean tree (or the
        // canonical checkout itself) answers the path, a tree aliasing the
        // canonical venv holds instead.
        let canon = tempfile::tempdir().unwrap();
        let wt = tempfile::tempdir().unwrap();
        let venv = wt.path().join("cli/.venv");
        std::fs::create_dir_all(&venv).unwrap();
        assert_eq!(
            canonical_venv_risk(wt.path(), Some(canon.path()), None),
            None
        );
        assert_eq!(
            canonical_venv_risk(canon.path(), Some(canon.path()), None),
            None
        );
        std::fs::remove_dir(&venv).unwrap();
        std::os::unix::fs::symlink(canon.path().join("cli/.venv"), &venv).unwrap();
        let risk = canonical_venv_risk(wt.path(), Some(canon.path()), None);
        assert!(
            risk.as_deref()
                .is_some_and(|r| r.contains("symlinks cli/.venv")),
            "{risk:?}"
        );
        std::fs::remove_file(&venv).unwrap();
        let env_canon: std::ffi::OsString = canon.path().join("cli/.venv").into();
        let risk = canonical_venv_risk(wt.path(), Some(canon.path()), Some(&env_canon));
        assert!(
            risk.as_deref()
                .is_some_and(|r| r.contains("UV_PROJECT_ENVIRONMENT")),
            "{risk:?}"
        );
        let env_elsewhere: std::ffi::OsString = "~/no-such-venv".into();
        assert_eq!(
            canonical_venv_risk(wt.path(), Some(canon.path()), Some(&env_elsewhere)),
            None
        );
    }

    #[test]
    fn a_picked_branch_rides_the_ensure_argv() {
        // AC8-HP: a non-empty payload branch appends `--branch <branch>` to
        // the ensure argv; the callers that name no branch (the node-seeded
        // spawn, the codex lane) keep today's exact argv.
        let _env = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        let args = dir.path().join("ensure-args");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\necho /wt/branch-target\n",
            args.display()
        );
        let bin = crate::write_exec_stub(dir.path(), "fno-py", &script);
        std::env::set_var("FNO_PY", &bin);
        let payload = json!({
            "recorded_cwd": dir.path().to_string_lossy(),
            "node": "x-eeee",
            "harness": "claude",
            "branch": "feature/x",
        });
        let (answer, _receipt) = decide(&payload);
        assert_eq!(answer, json!({ "workdir": "/wt/branch-target" }));
        let recorded = std::fs::read_to_string(&args).unwrap();
        let recorded: Vec<String> = recorded.lines().map(str::to_string).collect();
        let branch_pos = recorded
            .iter()
            .position(|a| a == "--branch")
            .expect("--branch rides the ensure argv");
        assert_eq!(
            recorded.get(branch_pos + 1).map(String::as_str),
            Some("feature/x")
        );
    }
}

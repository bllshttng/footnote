//! Children fno-agents spawns for lifecycle actions: the bounded
//! `claude stop`, and the all-source heal-token helper shellout. One module
//! answers one question - how every such child runs (cwd pinning, bounds,
//! output contract) - so a dead daemon cwd cannot take them all down at once
//! (: a daemon lazy-started from a worktree the reaper later deleted
//! kept that deleted cwd forever, and every child that inherited it died at
//! getcwd, leaving finished workers unstopable and holding fleet slots).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::client_verbs::{backfill_row_aliases, is_identity_token, py_repr_str};

/// The cwd a lifecycle child inherits: this process's cwd while getcwd still
/// resolves, else `fallback`, else the temp dir. These children need no
/// particular directory, only an existing one.
pub(crate) fn lifecycle_child_cwd_from(cwd: std::io::Result<PathBuf>, fallback: &Path) -> PathBuf {
    cwd.unwrap_or_else(|_| {
        if fallback.is_dir() {
            fallback.to_path_buf()
        } else {
            std::env::temp_dir()
        }
    })
}

pub(crate) fn lifecycle_child_cwd(fallback: &Path) -> PathBuf {
    lifecycle_child_cwd_from(std::env::current_dir(), fallback)
}

/// Run `claude stop <short>`, refusing to wait past `timeout`. `kill_on_drop`:
/// on timeout the `output()` future is dropped, so the hung child does not
/// keep running past the deadline this call gave up at (self-review finding:
/// this was hand-duplicated at the RPC call site in daemon.rs; one shared
/// helper now backs both).
pub(crate) async fn bounded_claude_stop(
    short: &str,
    timeout: Duration,
) -> Result<std::io::Result<std::process::Output>, tokio::time::error::Elapsed> {
    let stop = tokio::process::Command::new("claude")
        .arg("stop")
        .arg(short)
        .current_dir(lifecycle_child_cwd(
            crate::paths::AgentsHome::from_env().root(),
        ))
        .kill_on_drop(true)
        .output();
    tokio::time::timeout(timeout, stop).await
}

fn helper_registry_path(registry_path: &Path) -> std::io::Result<PathBuf> {
    let absolute = if registry_path.is_absolute() {
        registry_path.to_path_buf()
    } else {
        std::env::current_dir()?.join(registry_path)
    };
    if let Ok(canonical) = fs::canonicalize(&absolute) {
        return Ok(canonical);
    }
    if let (Some(parent), Some(name)) = (absolute.parent(), absolute.file_name()) {
        if let Ok(canonical_parent) = fs::canonicalize(parent) {
            return Ok(canonical_parent.join(name));
        }
    }
    Ok(absolute)
}

fn token_helper_args(token: &str, registry_path: &Path, cross_project: bool) -> Vec<String> {
    let mut args = vec![
        "agents".to_string(),
        "heal-token".to_string(),
        token.to_string(),
        "--registry".to_string(),
        registry_path.to_string_lossy().into_owned(),
        "--all-sources".to_string(),
    ];
    if cross_project {
        args.push("--cross-project".to_string());
    }
    args
}

fn token_helper_output(
    token: &str,
    registry_path: &Path,
    cross_project: bool,
    scope_cwd: Option<&Path>,
) -> std::io::Result<std::process::Output> {
    let registry_path = helper_registry_path(registry_path)?;
    let mut command = std::process::Command::new("fno");
    command
        .args(token_helper_args(token, &registry_path, cross_project))
        .env("FNO_AGENTS_RUNTIME", "python");
    match scope_cwd {
        Some(cwd) => command.current_dir(cwd),
        // No scope dir named: pin an existing cwd anyway, because a daemon
        // lazy-started from a worktree the reaper later deleted would hand the
        // child its own dead cwd, and the Python helper dies at getcwd with a
        // traceback. The registry's parent (~/.fno/agents) is the
        // nearest always-there directory.
        None => command.current_dir(lifecycle_child_cwd(
            registry_path.parent().unwrap_or_else(|| Path::new(".")),
        )),
    };
    command.output()
}

/// Ask the Python resolver to union registry and harness-store candidates.
///
/// `Ok(Some(row))` on resolution, `Ok(None)` only on the helper's documented
/// clean miss, and `Err(msg)` on ambiguity or unavailable/incomplete coverage.
/// `FNO_AGENTS_RUNTIME=python` pins the child to the Python dispatch so the
/// shellout cannot recurse back into this binary.
pub(crate) fn heal_token(
    token: &str,
    registry_path: &Path,
    cross_project: bool,
    scope_cwd: Option<&Path>,
) -> Result<Option<Value>, String> {
    let out = match token_helper_output(token, registry_path, cross_project, scope_cwd) {
        Ok(o) => o,
        Err(exc) => {
            return Err(format!(
                "cannot safely resolve token {} because the all-source identity helper could not run: {exc}. Use the full session id.",
                py_repr_str(token)
            ));
        }
    };
    // The healer adopts best-effort: a failed registry write still returns the
    // row, with the reason on stderr. Swallowing that would make the degradation
    // invisible -- the verb works, the roster silently does not.
    let parsed = parse_heal_token_output(token, &out);
    if matches!(&parsed, Ok(Some(_))) {
        let warn = String::from_utf8_lossy(&out.stderr);
        if !warn.trim().is_empty() {
            eprint!("{warn}");
        }
    }
    parsed
}

/// Enforce the Python helper's output contract without collapsing unavailable
/// coverage into a clean miss. Kept pure so malformed/off-contract subprocess
/// results are mechanically testable without mutating PATH.
fn parse_heal_token_output(
    token: &str,
    out: &std::process::Output,
) -> Result<Option<Value>, String> {
    const AMBIGUOUS: i32 = 3;
    const MISS: i32 = 13;

    if out.status.code() == Some(AMBIGUOUS) {
        let detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if detail.is_empty() {
            format!(
                "token {} is ambiguous across harness stores",
                py_repr_str(token)
            )
        } else {
            detail
        });
    }
    if out.status.code() == Some(MISS) {
        return Ok(None);
    }
    if !out.status.success() {
        // One labelled line, never the raw multi-line stderr (a traceback
        // ahead of the refusal is a new error class) - but the CAUSE line, the
        // helper's LAST stderr line, not its first. A Python traceback opens
        // with the useless "Traceback" header and names the real failure in
        // its tail ("OSError: ...deleted"); quoting the first line shipped
        // "(exit 1): Traceback", a refusal that names nothing.
        let stderr = String::from_utf8_lossy(&out.stderr);
        let cause = stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("");
        return Err(format!(
            "cannot safely resolve token {} because the all-source identity helper failed (exit {}){}. Use the full session id.",
            py_repr_str(token),
            out.status.code().unwrap_or(-1),
            if cause.is_empty() { String::new() } else { format!(": {}", cause.trim()) },
        ));
    }
    // The LAST non-empty line, not the whole buffer: a first-run `fno` may print
    // a setup-migration banner ahead of the payload.
    let text = String::from_utf8_lossy(&out.stdout);
    let line = match text.lines().rev().find(|l| !l.trim().is_empty()) {
        Some(l) => l,
        None => {
            return Err(format!(
                "cannot safely resolve token {} because the all-source identity helper returned no row. Use the full session id.",
                py_repr_str(token)
            ))
        }
    };
    match serde_json::from_str::<Value>(line) {
        Ok(mut row) if row.is_object() => {
            // The healed row skipped `load_registry_entries`, so it gets neither
            // that loader's alias reconciliation nor its validation. Apply both:
            // without the backfill the row has no `claude_session_uuid` (resume's
            // dead arm would refuse); without the field bar, an exit-0 helper returning `{}` or a
            // partial object would resolve as a SUCCESS and surface as a confusing
            // missing-cwd error three frames later instead of a clean not-found.
            let obj = match row.as_object_mut() {
                Some(o) => o,
                None => unreachable!("object guard above"),
            };
            backfill_row_aliases(obj, false);
            let has_identity = is_identity_token(obj.get("harness").and_then(Value::as_str));
            let has_fields = ["name", "cwd", "log_path"]
                .iter()
                .all(|k| obj.contains_key(*k));
            if !has_identity || !has_fields {
                return Err(format!(
                    "cannot safely resolve token {} because the all-source identity helper returned an incomplete row. Use the full session id.",
                    py_repr_str(token)
                ));
            }
            Ok(Some(row))
        }
        _ => Err(format!(
            "cannot safely resolve token {} because the all-source identity helper returned malformed JSON. Use the full session id.",
            py_repr_str(token)
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claims::test_env_lock;
    use crate::{path_with, PATH_TEST_MUTEX};

    /// a live cwd passes through untouched (the dominant case).
    #[test]
    fn lifecycle_child_cwd_passes_a_live_cwd_through() {
        let live = std::env::temp_dir().join("x8f73-live-cwd");
        std::fs::create_dir_all(&live).unwrap();
        let chosen = lifecycle_child_cwd_from(Ok(live.clone()), Path::new("/nonexistent"));
        assert_eq!(chosen, live);
        std::fs::remove_dir_all(&live).ok();
    }

    /// a dead cwd (getcwd errored) falls back to an existing fallback;
    /// a missing fallback degrades to the temp dir instead of handing the child
    /// another missing directory.
    #[test]
    fn lifecycle_child_cwd_falls_back_when_getcwd_fails() {
        let fallback = std::env::temp_dir().join("x8f73-fallback");
        std::fs::create_dir_all(&fallback).unwrap();
        assert_eq!(
            lifecycle_child_cwd_from(
                Err(std::io::Error::from_raw_os_error(libc::ENOENT)),
                &fallback
            ),
            fallback
        );
        std::fs::remove_dir_all(&fallback).ok();
        // The fallback itself is gone: temp dir, never a missing path.
        let missing = Path::new("/nonexistent/x8f73");
        assert_eq!(
            lifecycle_child_cwd_from(
                Err(std::io::Error::from_raw_os_error(libc::ENOENT)),
                missing
            ),
            std::env::temp_dir()
        );
    }

    #[test]
    fn heal_output_distinguishes_clean_miss_from_broken_coverage() {
        let miss = std::process::Command::new("sh")
            .args(["-c", "exit 13"])
            .output()
            .unwrap();
        assert!(parse_heal_token_output("deadbeef", &miss)
            .unwrap()
            .is_none());

        let off_contract = std::process::Command::new("sh")
            .args(["-c", "echo probe-broke >&2; exit 7"])
            .output()
            .unwrap();
        let message = parse_heal_token_output("deadbeef", &off_contract).unwrap_err();
        assert!(message.contains("cannot safely resolve"));
        assert!(message.contains("probe-broke"));

        let malformed = std::process::Command::new("sh")
            .args(["-c", "printf 'not-json\\n'"])
            .output()
            .unwrap();
        assert!(parse_heal_token_output("deadbeef", &malformed)
            .unwrap_err()
            .contains("malformed JSON"));
    }

    #[test]
    fn heal_failure_relays_the_cause_line_not_the_traceback_header() {
        // The failure shape: a Python traceback whose first line is the
        // useless header and whose tail names the real cause. The refusal
        // relays ONE labelled line - the tail - and never the raw buffer.
        let out = std::process::Command::new("sh")
            .args([
                "-c",
                "echo Traceback >&2; echo '  more' >&2; echo 'OSError: The current working directory was deleted' >&2; exit 1",
            ])
            .output()
            .unwrap();
        let message = parse_heal_token_output("deadbeef", &out).unwrap_err();
        assert!(!message.contains("Traceback"), "{message}");
        assert!(
            message.contains("OSError: The current working directory was deleted"),
            "{message}"
        );
        assert!(!message.contains('\n'), "{message}");
    }

    #[test]
    fn heal_token_helper_forwards_cross_project_exactly_once() {
        let registry = Path::new("/tmp/registry.json");
        assert_eq!(
            token_helper_args("deadbeef", registry, false),
            vec![
                "agents",
                "heal-token",
                "deadbeef",
                "--registry",
                "/tmp/registry.json",
                "--all-sources",
            ]
        );
        assert_eq!(
            token_helper_args("deadbeef", registry, true),
            vec![
                "agents",
                "heal-token",
                "deadbeef",
                "--registry",
                "/tmp/registry.json",
                "--all-sources",
                "--cross-project",
            ]
        );
    }

    fn marked_helper_fno(dir: &Path) {
        use std::os::unix::fs::PermissionsExt;

        let fake_fno = dir.join("fno");
        std::fs::write(
            &fake_fno,
            "#!/bin/sh\npwd > \"$FNO_TEST_HELPER_CWD\"\nprintf '%s\\n' \"$5\" > \"$FNO_TEST_HELPER_REGISTRY\"\nexit 0\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fake_fno).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake_fno, permissions).unwrap();
    }

    #[test]
    fn token_helper_runs_in_explicit_scope_cwd() {
        // PATH mutation is process-global: take the lib-wide test mutex so a
        // concurrent PATH-dependent test does not inherit this stub.
        let _path_guard = PATH_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _guard = test_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let scope = dir.path().join("replacement");
        std::fs::create_dir(&scope).unwrap();
        let marker = dir.path().join("helper-cwd");
        let registry_marker = dir.path().join("helper-registry");
        marked_helper_fno(dir.path());

        let old_path = std::env::var_os("PATH");
        let caller_cwd = std::env::current_dir().unwrap();
        let relative_registry = Path::new("relative/registry.json");
        let expected_registry = caller_cwd.join(relative_registry);
        std::env::set_var("PATH", path_with(dir.path()));
        std::env::set_var("FNO_TEST_HELPER_CWD", &marker);
        std::env::set_var("FNO_TEST_HELPER_REGISTRY", &registry_marker);
        let output =
            token_helper_output("deadbeef", relative_registry, false, Some(&scope)).unwrap();
        match old_path {
            Some(path) => std::env::set_var("PATH", path),
            None => std::env::remove_var("PATH"),
        }
        std::env::remove_var("FNO_TEST_HELPER_CWD");
        std::env::remove_var("FNO_TEST_HELPER_REGISTRY");

        assert!(output.status.success());
        let observed = PathBuf::from(std::fs::read_to_string(&marker).unwrap().trim())
            .canonicalize()
            .unwrap();
        assert_eq!(observed, scope.canonicalize().unwrap());
        assert_eq!(
            Path::new(std::fs::read_to_string(registry_marker).unwrap().trim()),
            expected_registry
        );
    }

    #[test]
    fn token_helper_without_scope_cwd_pins_an_existing_directory() {
        // with no scope dir named, the child inherits the caller's cwd
        // - fatal when a daemon lazy-started from a worktree the reaper later
        // deleted keeps that dead cwd forever. The helper child must land in an
        // existing directory (the registry's parent) instead.
        let _path_guard = PATH_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _guard = test_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().unwrap();
        crate::claims::pin_test_claims_root(dir.path());
        let marker = dir.path().join("helper-cwd");
        let registry_marker = dir.path().join("helper-registry");
        marked_helper_fno(dir.path());

        let old_path = std::env::var_os("PATH");
        let registry = dir.path().canonicalize().unwrap().join("registry.json");
        std::env::set_var("PATH", path_with(dir.path()));
        std::env::set_var("FNO_TEST_HELPER_CWD", &marker);
        std::env::set_var("FNO_TEST_HELPER_REGISTRY", &registry_marker);
        let output = token_helper_output("deadbeef", &registry, false, None).unwrap();
        match old_path {
            Some(path) => std::env::set_var("PATH", path),
            None => std::env::remove_var("PATH"),
        }
        std::env::remove_var("FNO_TEST_HELPER_CWD");
        std::env::remove_var("FNO_TEST_HELPER_REGISTRY");

        assert!(output.status.success());
        let observed = PathBuf::from(std::fs::read_to_string(&marker).unwrap().trim())
            .canonicalize()
            .unwrap();
        // The child's cwd is the chooser's answer: the live cwd when getcwd
        // works, else the registry's parent (nearest always-there directory).
        // The fallback arm itself is pinned by the pure
        // lifecycle_child_cwd_from tests, since chdir is process-global.
        let expected = lifecycle_child_cwd(registry.parent().unwrap_or(Path::new(".")));
        assert_eq!(observed, expected.canonicalize().unwrap());
        assert_eq!(
            Path::new(std::fs::read_to_string(registry_marker).unwrap().trim()),
            registry
        );
    }
}

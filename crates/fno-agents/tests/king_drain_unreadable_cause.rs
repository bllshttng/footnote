//! The quiet-board drain failure must name its cause. A killed drain child
//! is a timeout waiting for a quieter fire; a failed drain command is a bug
//! to debug. Both blocked completion before, but the message flattened them
//! into one cause-free "unreadable".

use fno_agents::loopcheck::run_loop_check_capture;
use serde_json::Value;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

struct EnvGuard {
    key: &'static str,
    prior: Option<OsString>,
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.prior {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

fn set_env(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> EnvGuard {
    let guard = EnvGuard {
        key,
        prior: std::env::var_os(key),
    };
    std::env::set_var(key, value);
    guard
}

fn write_exec(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// The king_quiet_scope fixture with the drain fake swapped for `body`:
/// quiet board, resolving epic scope, fast board subprocesses, and a
/// `--read-timeout-ms` small enough that a hanging drain dies inside it.
/// The env guards must outlive the fire, so the caller drops them.
fn quiet_fire(body: &str) -> (tempfile::TempDir, [EnvGuard; 5], Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let graph = home.join("graph.json");
    std::fs::write(
        &graph,
        r#"{"entries":[{"id":"x-epic","type":"epic","priority":"p1","status":"done"}]}"#,
    )
    .unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[paths]\ngraph_json = {:?}\n[work.workspaces.test]\nprojects = [{{name = \"fno\"}}]\n",
            graph.to_string_lossy()
        ),
    )
    .unwrap();
    write_exec(&bin, "fno-py", "#!/bin/sh\nprintf '[]\\n'");
    let gh = write_exec(&bin, "gh", "#!/bin/sh\nprintf '[]\\n'");
    let fno = write_exec(dir.path(), "fno-drain", body);
    let state = dir.path().join("king.md");
    std::fs::write(
        &state,
        "---\nfno_id: k-1\nscope: x-epic\ncreated_at: 2026-09-06T00:00:00Z\n---\n",
    )
    .unwrap();
    let events = dir.path().join("events.jsonl");
    std::fs::write(&events, "").unwrap();
    let old_path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(bin.clone().into_os_string())
            .chain(std::env::split_paths(&old_path).map(OsString::from)),
    )
    .unwrap();
    let env = [
        set_env("FNO_HOME", &home),
        set_env("FNO_CONFIG", &config),
        set_env("FNO_AGENTS_HOME", dir.path().join("agents")),
        set_env("FNO_CLAIMS_ROOT", dir.path().join("claims")),
        set_env("PATH", path),
    ];

    let args = [
        "loop-check",
        "--state",
        state.to_str().unwrap(),
        "--transcript",
        dir.path().join("transcript").to_str().unwrap(),
        "--cwd",
        dir.path().to_str().unwrap(),
        "--events",
        events.to_str().unwrap(),
        "--global-events",
        events.to_str().unwrap(),
        "--settings",
        config.to_str().unwrap(),
        "--global-settings",
        config.to_str().unwrap(),
        "--ledger",
        dir.path().join("ledger.json").to_str().unwrap(),
        "--gh-bin",
        gh.to_str().unwrap(),
        "--git-bin",
        "git",
        "--author-harness",
        "none",
        "--driver",
        "king",
        "--fno-bin",
        fno.to_str().unwrap(),
        "--read-timeout-ms",
        "2000",
    ]
    .into_iter()
    .map(str::to_string)
    .collect::<Vec<_>>();
    (dir, env, args)
}

#[test]
fn a_killed_drain_names_its_timeout_never_a_bare_unreadable() {
    let (_dir, _env, args) = quiet_fire("#!/bin/sh\nsleep 5\n");
    let (code, output) = run_loop_check_capture(&args);
    let payload: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(code, 0, "{output}");
    assert_eq!(payload["decision"], "block", "{output}");
    let reason = payload["reason"].as_str().unwrap();
    assert!(
        reason.contains("timed out"),
        "a killed drain must say it timed out: {reason}"
    );
    assert!(
        reason.contains("drain"),
        "the message must name the drain read: {reason}"
    );
    assert!(
        reason.contains("blocking completion"),
        "the gate must stay fail-closed: {reason}"
    );
}

#[test]
fn a_failed_drain_quotes_the_command_failure() {
    let (_dir, _env, args) =
        quiet_fire("#!/bin/sh\necho 'collector exploded: cannot compile scope' >&2\nexit 3\n");
    let (code, output) = run_loop_check_capture(&args);
    let payload: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(code, 0, "{output}");
    assert_eq!(payload["decision"], "block", "{output}");
    let reason = payload["reason"].as_str().unwrap();
    assert!(
        reason.contains("collector exploded"),
        "a failed drain must quote the command failure: {reason}"
    );
    assert!(
        reason.contains("blocking completion"),
        "the gate must stay fail-closed: {reason}"
    );
}

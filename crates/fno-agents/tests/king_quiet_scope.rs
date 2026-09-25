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

/// A quiet board with undelivered scope may stop while it waits for CI or a
/// worker; the next beat or delivery event wakes it again.
#[test]
fn a_quiet_board_with_undelivered_scope_stops_while_waiting() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let graph = home.join("graph.json");
    fno_agents::graph_store::seed_rows(
        &graph,
        &[serde_json::json!({
            "id": "x-epic", "slug": "x-epic", "title": "epic", "type": "epic",
            "priority": "p1", "status": "done", "completed_at": "2026-08-18T00:00:00Z"
        })],
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
    write_exec(
        &bin,
        "fno-py",
        "#!/bin/sh\nprintf '{\"questions\":[],\"verdicts\":{}}\\n'",
    );
    let gh = write_exec(&bin, "gh", "#!/bin/sh\nprintf '[]\\n'");
    let fno = write_exec(
        dir.path(),
        "fno-drain",
        "#!/bin/sh\nprintf '{\"scope\":\"x-epic\",\"undelivered\":4}\\n'",
    );
    let state = dir.path().join("king.md");
    std::fs::write(
        &state,
        format!(
            "---\nfno_id: k-1\nscope: x-epic\ncreated_at: {}\n---\n",
            (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339()
        ),
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
    let _env = [
        set_env("FNO_HOME", &home),
        set_env("FNO_CONFIG", &config),
        set_env("FNO_AGENTS_HOME", dir.path().join("agents")),
        set_env("FNO_CLAIMS_ROOT", dir.path().join("claims")),
        set_env("FNO_SPACES_DIR", dir.path().join("spaces")),
        set_env("FNO_PY", bin.join("fno-py")),
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

    let (code, output) = run_loop_check_capture(&args);
    let payload: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(code, 0, "wait fire: {output}");
    assert_eq!(payload["decision"], "allow", "wait fire: {output}");
    assert_eq!(
        payload["termination_reason"], "NoWork",
        "the quiet board may stop while waiting: {output}"
    );
    assert!(
        payload["reason"]
            .as_str()
            .is_some_and(|reason| reason.starts_with("waiting on CI or a worker")),
        "the wait must name why: {output}"
    );
}

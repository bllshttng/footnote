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

#[test]
fn a_quiet_board_with_undelivered_scope_stays_in_flight() {
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
    let fno = write_exec(
        dir.path(),
        "fno-drain",
        "#!/bin/sh\nprintf '{\"scope\":\"x-epic\",\"undelivered\":4}\\n'",
    );
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
    let _env = [
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

    for fire in 0..3 {
        let (code, output) = run_loop_check_capture(&args);
        let payload: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(code, 0, "fire {fire}: {output}");
        assert_eq!(payload["decision"], "block", "fire {fire}: {output}");
        assert!(
            payload["termination_reason"].is_null(),
            "fire {fire}: {output}"
        );
        assert!(
            payload["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("undelivered")),
            "fire {fire}: {output}"
        );
    }
}

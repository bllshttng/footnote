//! Integration tests for `claude_supervisor::guard_birth` against a fake
//! `claude`. The guard's runtime branches - birth with a clean env,
//! and no-birth when a supervisor already serves the dir - are driven for
//! real: the fake records the env a born `daemon run` would carry, which is
//! the AC3 assertion, without touching any real config dir or daemon.

use fno_agents::claude_supervisor::guard_birth;
use std::fs;
use std::path::{Path, PathBuf};

/// Serializes the tests that mutate process env (the lib's
/// `claims::test_env_lock` is #[cfg(test)]-gated).
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn tmpdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "fno-sup-birth-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&p).unwrap();
    p
}

/// Fake `claude`: `daemon status` answers from a state file; `daemon run`
/// dumps its whole env (the birth env the real supervisor would carry) and
/// touches the state file so the guard's status poll goes green.
fn install_fake_claude(bin_dir: &Path) {
    let script = r#"#!/bin/sh
if [ "$1" = "daemon" ] && [ "$2" = "status" ]; then
  if [ -f "$FAKE_STATE" ]; then exit 0; fi
  exit 1
fi
if [ "$1" = "daemon" ] && [ "$2" = "run" ]; then
  env | sort > "$FAKE_BIRTH_ENV_DUMP"
  touch "$FAKE_STATE"
  sleep 30
  exit 0
fi
exit 0
"#;
    let path = bin_dir.join("claude");
    fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn path_with(bin_dir: &Path) -> String {
    format!("{}:/usr/bin:/bin", bin_dir.display())
}

#[test]
fn birth_under_a_dirty_env_is_clean() {
    // AC3/AC1 runtime half: a poisoned shell + no supervisor. The guard must
    // birth the supervisor with every poison key held back, keeping the
    // config dir that selects it.
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let bin = tmpdir("dirty-bin");
    install_fake_claude(&bin);
    let dir = tmpdir("dirty-dir");
    let dump = dir.join("birth-env.txt");
    let state = dir.join("state");

    let poison = [
        "FNO_AGENTS_RUNTIME",
        "FNO_WAKE_MSG",
        "FNO_ROUTE_PROVIDER",
        "FNO_PLAN_PROBE",
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_MODEL",
        "CLAUDE_PID",
        "CODEX_COMPANION_SESSION_ID",
    ];
    let prior: Vec<_> = poison.iter().map(|k| (*k, std::env::var(k).ok())).collect();
    for k in poison {
        std::env::set_var(k, "probe-value");
    }
    let prior_path = std::env::var("PATH").ok();
    std::env::set_var("PATH", path_with(&bin));
    // The knobs the fake reads must ride the PARENT env: the guard strips
    // FNO_*/poison keys off the child, but these FAKE_* names are not poison,
    // so setting them here reaches the born daemon run.
    std::env::set_var("FAKE_STATE", &state);
    std::env::set_var("FAKE_BIRTH_ENV_DUMP", &dump);

    guard_birth([("CLAUDE_CONFIG_DIR", dir.to_str().unwrap())]);
    // Restore before asserting so a failure does not leak the poison.
    std::env::remove_var("FAKE_STATE");
    std::env::remove_var("FAKE_BIRTH_ENV_DUMP");
    for (k, v) in prior {
        match v {
            Some(v) => std::env::set_var(k, v),
            None => std::env::remove_var(k),
        }
    }
    match prior_path {
        Some(v) => std::env::set_var("PATH", v),
        None => std::env::remove_var("PATH"),
    }

    let env_dump =
        fs::read_to_string(&dump).unwrap_or_else(|e| panic!("daemon run never ran: {e}"));
    for line in env_dump.lines() {
        for k in poison {
            assert!(
                !line.starts_with(&format!("{k}=")),
                "the born supervisor carried poison key {k}: {line}"
            );
        }
    }
    assert!(
        env_dump
            .lines()
            .any(|l| l == format!("CLAUDE_CONFIG_DIR={}", dir.display())),
        "the born supervisor must keep its config dir: {env_dump}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn no_second_birth_when_a_supervisor_serves_the_dir() {
    // AC4-EDGE: status green up front -> the guard starts nothing.
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let bin = tmpdir("running-bin");
    install_fake_claude(&bin);
    let dir = tmpdir("running-dir");
    let state = dir.join("state");
    let dump = dir.join("birth-env.txt");
    fs::write(&state, "").unwrap();

    let prior_path = std::env::var("PATH").ok();
    std::env::set_var("PATH", path_with(&bin));
    std::env::set_var("FAKE_STATE", &state);
    guard_birth([("CLAUDE_CONFIG_DIR", dir.to_str().unwrap())]);
    match prior_path {
        Some(v) => std::env::set_var("PATH", v),
        None => std::env::remove_var("PATH"),
    }
    std::env::remove_var("FAKE_STATE");

    assert!(
        !dump.exists(),
        "guard_birth started a second supervisor beside a running one"
    );
    let _ = fs::remove_dir_all(&dir);
}
